//! Minimal DRM syncobj shim for `wp_linux_drm_syncobj_v1`.
//!
//! Explicit sync exists because implicit dma-buf fencing cannot be relied
//! on: NVIDIA's Vulkan driver neither attaches nor honors reservation
//! fences on imported dma-bufs, so without this protocol a client's GPU
//! write races the compositor's sampling and the composite shows the
//! buffer's previous contents.  The client hands us a DRM timeline
//! syncobj; each commit carries an acquire point (wait for it before
//! reading the buffer) and a release point (signal it once the read is
//! done).
//!
//! Everything here is raw ioctls against the render node — no drm crate
//! dependency for four calls.

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;

const DRM_IOCTL_BASE: u64 = b'd' as u64;

const fn drm_iowr(nr: u64, size: usize) -> u64 {
    // _IOC(dir, type, nr, size): dir = read|write = 3.
    (3u64 << 30) | ((size as u64) << 16) | (DRM_IOCTL_BASE << 8) | nr
}

#[repr(C)]
struct DrmGetCap {
    capability: u64,
    value: u64,
}

#[repr(C)]
struct DrmSyncobjDestroy {
    handle: u32,
    pad: u32,
}

#[repr(C)]
struct DrmSyncobjHandle {
    handle: u32,
    flags: u32,
    fd: i32,
    pad: u32,
}

#[repr(C)]
struct DrmSyncobjTimelineWait {
    handles: u64,
    points: u64,
    timeout_nsec: i64,
    count_handles: u32,
    flags: u32,
    first_signaled: u32,
    pad: u32,
}

#[repr(C)]
struct DrmSyncobjTimelineArray {
    handles: u64,
    points: u64,
    count_handles: u32,
    flags: u32,
}

const DRM_IOCTL_GET_CAP: u64 = drm_iowr(0x0c, std::mem::size_of::<DrmGetCap>());
const DRM_IOCTL_SYNCOBJ_DESTROY: u64 = drm_iowr(0xC0, std::mem::size_of::<DrmSyncobjDestroy>());
const DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE: u64 =
    drm_iowr(0xC2, std::mem::size_of::<DrmSyncobjHandle>());
const DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT: u64 =
    drm_iowr(0xCA, std::mem::size_of::<DrmSyncobjTimelineWait>());
const DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL: u64 =
    drm_iowr(0xCD, std::mem::size_of::<DrmSyncobjTimelineArray>());

const DRM_CAP_SYNCOBJ_TIMELINE: u64 = 0x14;

/// An open render node used only for syncobj ioctls.
pub(crate) struct DrmSyncobjDevice {
    fd: OwnedFd,
}

impl DrmSyncobjDevice {
    /// Open `path` and confirm the kernel driver supports timeline
    /// syncobjs; `None` disables the protocol global entirely.
    pub(crate) fn open(path: &str) -> Option<Arc<Self>> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .ok()?;
        let fd: OwnedFd = file.into();
        let mut cap = DrmGetCap {
            capability: DRM_CAP_SYNCOBJ_TIMELINE,
            value: 0,
        };
        let r = unsafe { libc::ioctl(fd.as_raw_fd(), DRM_IOCTL_GET_CAP, &mut cap) };
        if r != 0 || cap.value == 0 {
            return None;
        }
        Some(Arc::new(Self { fd }))
    }

    fn ioctl<T>(&self, nr: u64, arg: &mut T) -> std::io::Result<()> {
        // DRM ioctls restart on EINTR/EAGAIN by convention.
        loop {
            let r = unsafe { libc::ioctl(self.fd.as_raw_fd(), nr, arg as *mut T) };
            if r == 0 {
                return Ok(());
            }
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EINTR) | Some(libc::EAGAIN) => continue,
                _ => return Err(err),
            }
        }
    }

    /// Import a client's syncobj fd, returning a timeline whose DRM
    /// handle stays valid for as long as the returned value lives.
    pub(crate) fn import_timeline(
        self: &Arc<Self>,
        fd: OwnedFd,
    ) -> std::io::Result<Arc<SyncobjTimeline>> {
        let mut arg = DrmSyncobjHandle {
            handle: 0,
            flags: 0,
            fd: fd.as_raw_fd(),
            pad: 0,
        };
        self.ioctl(DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE, &mut arg)?;
        Ok(Arc::new(SyncobjTimeline {
            device: self.clone(),
            handle: arg.handle,
        }))
    }
}

/// An imported client timeline.  Pending acquire/release uses hold `Arc`
/// clones, so the DRM handle outlives the protocol object when needed.
pub(crate) struct SyncobjTimeline {
    device: Arc<DrmSyncobjDevice>,
    handle: u32,
}

impl SyncobjTimeline {
    /// Whether `point` has materialized *and* signalled.  A zero-timeout
    /// wait without WAIT_FOR_SUBMIT reports exactly that: ETIME while
    /// pending, EINVAL while the point has no fence yet.
    pub(crate) fn point_signaled(&self, point: u64) -> bool {
        let handles = [self.handle];
        let points = [point];
        let mut arg = DrmSyncobjTimelineWait {
            handles: handles.as_ptr() as u64,
            points: points.as_ptr() as u64,
            timeout_nsec: 0,
            count_handles: 1,
            flags: 0,
            first_signaled: 0,
            pad: 0,
        };
        self.device
            .ioctl(DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT, &mut arg)
            .is_ok()
    }

    /// Signal `point` from the CPU.  Used once the GPU work that read the
    /// buffer has fence-completed (or when the buffer is discarded
    /// unread), which makes a CPU signal exact rather than early.
    pub(crate) fn signal_point(&self, point: u64) {
        let handles = [self.handle];
        let points = [point];
        let mut arg = DrmSyncobjTimelineArray {
            handles: handles.as_ptr() as u64,
            points: points.as_ptr() as u64,
            count_handles: 1,
            flags: 0,
        };
        if let Err(e) = self
            .device
            .ioctl(DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL, &mut arg)
        {
            eprintln!("[drm-syncobj] TIMELINE_SIGNAL failed: {e}");
        }
    }
}

impl Drop for SyncobjTimeline {
    fn drop(&mut self) {
        let mut arg = DrmSyncobjDestroy {
            handle: self.handle,
            pad: 0,
        };
        let _ = self.device.ioctl(DRM_IOCTL_SYNCOBJ_DESTROY, &mut arg);
    }
}

/// One end of a commit's synchronization: a timeline plus a point on it.
#[derive(Clone)]
pub(crate) struct SyncPoint {
    pub timeline: Arc<SyncobjTimeline>,
    pub point: u64,
}

impl SyncPoint {
    pub(crate) fn signaled(&self) -> bool {
        self.timeline.point_signaled(self.point)
    }

    pub(crate) fn signal(&self) {
        self.timeline.signal_point(self.point);
    }
}
