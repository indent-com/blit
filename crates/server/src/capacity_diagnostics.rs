//! Pure startup capacity-plan calculation and rendering.
//!
//! The integration point deliberately supplies only the once-sampled
//! deployment resolver and feature gates. Keeping the arithmetic here makes
//! it difficult for diagnostics to drift into per-attempt-only numbers or to
//! describe a configuration plan as a hard resident-memory limit.

const MIB: u128 = 1024 * 1024;
const GIB: u128 = 1024 * MIB;

const DEFAULT_MAX_TRANSIENT: u64 = 128;
const DEFAULT_MAX_VALIDATING: u64 = 2;
const DEFAULT_MEMORY_BYTES: u64 = 128 * 1024 * 1024;
const DEFAULT_NATIVE_STACK_BYTES: u64 = 2 * 1024 * 1024;
const DEFAULT_OUTBOX_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_ARGUMENT_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_OUTPUT_RETAIN_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_COMMAND_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_JOB_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_MODULE_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_CHANNEL_LISTENERS: u64 = 1_024;
const DEFAULT_CHANNEL_PAIRS: u64 = 128;
const DEFAULT_CHANNEL_BUFFER_BYTES: u64 = 256 * 1024 * 1024;

// Fixed protocol/implementation maxima which participate in the planning
// subtotal. These are inputs to memory planning, not deployment knobs.
const PACKET_BYTES: u128 = 16 * MIB;
const DUPLEX_PRIVATE_LENGTH_BYTES: u128 = 4;
const CHANNEL_METADATA_BYTES: u128 = 64 * 1024;
const CHANNEL_PEER_BYTES: u128 = 255;
const CHANNEL_WINDOW_BYTES: u128 = MIB;
const TERMINAL_STATUS_BYTES: u128 = 91 + 4 * 1024;
const TERMINAL_EXIT_BYTES: u128 = 50 + 4 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CapacityInputs {
    pub extensions_enabled: bool,
    pub channels_enabled: bool,
    pub max_running: u64,
    pub max_transient: u64,
    pub max_validating: u64,
    pub memory_bytes: u64,
    pub native_stack_bytes: u64,
    pub outbox_bytes: u64,
    pub argument_bytes: u64,
    pub output_retain_bytes: u64,
    pub command_bytes: u64,
    pub job_bytes: u64,
    pub module_bytes: u64,
    pub channel_listeners: u64,
    pub channel_pairs: u64,
    pub channel_buffer_bytes: u64,
}

impl CapacityInputs {
    /// Sample all relevant settings through the same frozen deployment
    /// resolver used by the owning subsystems.
    pub(crate) fn sample(
        extensions_enabled: bool,
        channels_enabled: bool,
        logical_cpus: usize,
        mut setting: impl FnMut(&'static str, u64) -> u64,
    ) -> Self {
        let max_running_default = logical_cpus.saturating_sub(1).clamp(1, 4) as u64;
        Self {
            extensions_enabled,
            channels_enabled,
            max_running: setting("BLIT_EXT_MAX_RUNNING", max_running_default).clamp(1, 4),
            max_transient: setting("BLIT_EXT_MAX_TRANSIENT", DEFAULT_MAX_TRANSIENT),
            max_validating: setting("BLIT_EXT_MAX_VALIDATING", DEFAULT_MAX_VALIDATING).max(1),
            memory_bytes: setting("BLIT_EXT_MEMORY_MAX", DEFAULT_MEMORY_BYTES),
            native_stack_bytes: setting("BLIT_EXT_STACK_SIZE", DEFAULT_NATIVE_STACK_BYTES),
            outbox_bytes: setting("BLIT_EXT_OUTBOX_MAX", DEFAULT_OUTBOX_BYTES),
            argument_bytes: setting("BLIT_EXT_ARGUMENT_STORE_MAX", DEFAULT_ARGUMENT_BYTES),
            output_retain_bytes: setting("BLIT_EXT_OUTPUT_RETAIN_MAX", DEFAULT_OUTPUT_RETAIN_BYTES),
            command_bytes: setting("BLIT_EXT_COMMAND_STORE_MAX", DEFAULT_COMMAND_BYTES),
            job_bytes: setting("BLIT_EXT_JOB_BYTES_MAX", DEFAULT_JOB_BYTES),
            module_bytes: setting("BLIT_EXT_MODULE_MAX", DEFAULT_MODULE_BYTES),
            channel_listeners: setting("BLIT_CHANNEL_MAX_LISTENERS", DEFAULT_CHANNEL_LISTENERS),
            channel_pairs: setting("BLIT_CHANNEL_MAX_CONNECTED", DEFAULT_CHANNEL_PAIRS),
            channel_buffer_bytes: setting("BLIT_CHANNEL_BUFFER_MAX", DEFAULT_CHANNEL_BUFFER_BYTES),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExtensionCapacity {
    pub enabled: bool,
    pub running_attempts: u128,
    pub queued_egress: u128,
    pub validation_inputs: u128,
    pub network_validation_requests: u128,
    pub retained_arguments: u128,
    pub retained_output: u128,
    pub terminal_records: u128,
    pub command_records: u128,
    pub tracked_job_requests: u128,
    pub subtotal: u128,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ChannelCapacity {
    pub enabled: bool,
    pub window_reservations: u128,
    pub listener_metadata: u128,
    pub connected_metadata: u128,
    pub peer_labels: u128,
    pub subtotal: u128,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CapacityPlan {
    pub extensions: ExtensionCapacity,
    pub channels: ChannelCapacity,
    pub combined: u128,
    pub max_running: u64,
    pub max_transient: u64,
    pub max_validating: u64,
    pub channel_listeners: u64,
    pub configured_channel_pairs: u64,
    pub admitted_channel_pairs: u64,
}

pub(crate) fn calculate(inputs: CapacityInputs) -> CapacityPlan {
    let running = u128::from(inputs.max_running);
    let per_attempt = u128::from(inputs.memory_bytes)
        + u128::from(inputs.native_stack_bytes)
        // One fixed-capacity in-process duplex buffer in each direction.
        + running_buffer_bytes()
        // One fixed-capacity host-adapter handoff in each direction.
        + 2 * PACKET_BYTES;
    let running_attempts = running * per_attempt;
    let queued_egress = running * u128::from(inputs.outbox_bytes);
    let validation_inputs = u128::from(inputs.max_validating) * u128::from(inputs.module_bytes);
    // Network packets are not retained by the extension-origin tracked-job
    // budget. One separate global request-byte lane bounds the packet which
    // feeds detached FINAL validation.
    let network_validation_requests = u128::from(inputs.module_bytes);
    let terminal_records =
        u128::from(inputs.max_transient) * (TERMINAL_STATUS_BYTES + TERMINAL_EXIT_BYTES);
    let extension_configured = running_attempts
        + queued_egress
        + validation_inputs
        + network_validation_requests
        + u128::from(inputs.argument_bytes)
        + u128::from(inputs.output_retain_bytes)
        + terminal_records
        + u128::from(inputs.command_bytes)
        + u128::from(inputs.job_bytes);
    let extension_subtotal = if inputs.extensions_enabled {
        extension_configured
    } else {
        0
    };

    let listener_metadata = u128::from(inputs.channel_listeners) * CHANNEL_METADATA_BYTES;
    let admitted_channel_pairs = inputs.channel_pairs.min(
        (u128::from(inputs.channel_buffer_bytes) / (2 * CHANNEL_WINDOW_BYTES))
            .min(u128::from(u64::MAX)) as u64,
    );
    // Each newly connected pair can retain both maximum-size metadata-bearing
    // OPENED/ACCEPTED notifications until their endpoint writers drain.
    let connected_metadata = u128::from(admitted_channel_pairs) * 2 * CHANNEL_METADATA_BYTES;
    let peer_labels = u128::from(admitted_channel_pairs) * 2 * CHANNEL_PEER_BYTES;
    let window_reservations = u128::from(admitted_channel_pairs) * 2 * CHANNEL_WINDOW_BYTES;
    let channel_configured =
        window_reservations + listener_metadata + connected_metadata + peer_labels;
    let channel_subtotal = if inputs.channels_enabled {
        channel_configured
    } else {
        0
    };

    CapacityPlan {
        extensions: ExtensionCapacity {
            enabled: inputs.extensions_enabled,
            running_attempts,
            queued_egress,
            validation_inputs,
            network_validation_requests,
            retained_arguments: u128::from(inputs.argument_bytes),
            retained_output: u128::from(inputs.output_retain_bytes),
            terminal_records,
            command_records: u128::from(inputs.command_bytes),
            tracked_job_requests: u128::from(inputs.job_bytes),
            subtotal: extension_subtotal,
        },
        channels: ChannelCapacity {
            enabled: inputs.channels_enabled,
            window_reservations,
            listener_metadata,
            connected_metadata,
            peer_labels,
            subtotal: channel_subtotal,
        },
        combined: extension_subtotal + channel_subtotal,
        max_running: inputs.max_running,
        max_transient: inputs.max_transient,
        max_validating: inputs.max_validating,
        channel_listeners: inputs.channel_listeners,
        configured_channel_pairs: inputs.channel_pairs,
        admitted_channel_pairs,
    }
}

pub(crate) fn sampled_diagnostic(
    extensions_enabled: bool,
    channels_enabled: bool,
    logical_cpus: usize,
    setting: impl FnMut(&'static str, u64) -> u64,
) -> String {
    render(calculate(CapacityInputs::sample(
        extensions_enabled,
        channels_enabled,
        logical_cpus,
        setting,
    )))
}

const fn running_buffer_bytes() -> u128 {
    2 * (PACKET_BYTES + DUPLEX_PRIVATE_LENGTH_BYTES)
}

pub(crate) fn render(plan: CapacityPlan) -> String {
    let extension_state = if plan.extensions.enabled {
        format_plan_bytes(plan.extensions.subtotal)
    } else {
        "disabled (active subtotal 0 B)".to_owned()
    };
    let channel_state = if plan.channels.enabled {
        format_plan_bytes(plan.channels.subtotal)
    } else {
        "disabled (active subtotal 0 B)".to_owned()
    };
    format!(
        "blit-server: startup capacity plan (sampled configuration)\n\
         extensions subtotal: {extension_state}; running attempts {} ({} max); queued egress {}; validation inputs {} ({} max); network validation requests {}; retained arguments {}; retained output {}; terminal replay reserve {} ({} bytes, derived from {} transients); command records/snapshots {}; tracked-job requests {}\n\
         channel reservation/metadata subtotal: {channel_state}; window reservations {} ({} admitted of {} configured pair slots); listener metadata {} ({} max listeners); connected-pair metadata {}; peer labels {}\n\
         combined plan: {}\n\
        caveat: this is a configuration-derived planning subtotal, not a hard RSS ceiling; resident Wasmi Module/Engine, table and value storage, framing scratch, allocator metadata/fragmentation, queue nodes, native-backend allocations, and kernel buffers are unaccounted, and Wasmi provides no exact aggregate-RSS limiter",
        format_plan_bytes(plan.extensions.running_attempts),
        plan.max_running,
        format_plan_bytes(plan.extensions.queued_egress),
        format_plan_bytes(plan.extensions.validation_inputs),
        plan.max_validating,
        format_plan_bytes(plan.extensions.network_validation_requests),
        format_plan_bytes(plan.extensions.retained_arguments),
        format_plan_bytes(plan.extensions.retained_output),
        format_plan_bytes(plan.extensions.terminal_records),
        plan.extensions.terminal_records,
        plan.max_transient,
        format_plan_bytes(plan.extensions.command_records),
        format_plan_bytes(plan.extensions.tracked_job_requests),
        format_plan_bytes(plan.channels.window_reservations),
        plan.admitted_channel_pairs,
        plan.configured_channel_pairs,
        format_plan_bytes(plan.channels.listener_metadata),
        plan.channel_listeners,
        format_plan_bytes(plan.channels.connected_metadata),
        format_plan_bytes(plan.channels.peer_labels),
        format_plan_bytes(plan.combined),
    )
}

fn format_plan_bytes(bytes: u128) -> String {
    if bytes >= GIB {
        format!(
            "{} GiB ({} MiB)",
            format_decimal(bytes as f64 / GIB as f64, 2),
            format_decimal(bytes as f64 / MIB as f64, 1)
        )
    } else if bytes >= MIB {
        let precision = usize::from(bytes < 10 * MIB) + 1;
        format!(
            "{} MiB",
            format_decimal(bytes as f64 / MIB as f64, precision)
        )
    } else if bytes >= 1024 {
        format!("{} KiB", format_decimal(bytes as f64 / 1024.0, 1))
    } else {
        format!("{bytes} B")
    }
}

fn format_decimal(value: f64, precision: usize) -> String {
    let raw = format!("{value:.precision$}");
    let (integer, fraction) = raw.split_once('.').unwrap_or((&raw, ""));
    let mut grouped = String::with_capacity(raw.len() + integer.len() / 3);
    for (index, byte) in integer.bytes().enumerate() {
        if index != 0 && (integer.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(char::from(byte));
    }
    if !fraction.is_empty() {
        grouped.push('.');
        grouped.push_str(fraction);
    }
    grouped
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn defaults(logical_cpus: usize) -> CapacityInputs {
        CapacityInputs::sample(true, true, logical_cpus, |_, default| default)
    }

    #[test]
    fn four_attempt_defaults_match_the_rfc_planning_example() {
        let plan = calculate(defaults(8));
        assert_eq!(plan.extensions.running_attempts, 776 * MIB + 32);
        assert_eq!(plan.extensions.queued_egress, 256 * MIB);
        assert_eq!(plan.extensions.validation_inputs, 128 * MIB);
        assert_eq!(plan.extensions.network_validation_requests, 64 * MIB);
        assert_eq!(plan.extensions.terminal_records, 1_066_624);
        assert_eq!(plan.extensions.subtotal, 1_754_285_728);
        assert_eq!(plan.channels.window_reservations, 256 * MIB);
        assert_eq!(plan.channels.listener_metadata, 64 * MIB);
        assert_eq!(plan.channels.connected_metadata, 16 * MIB);
        assert_eq!(plan.channels.peer_labels, 65_280);
        assert_eq!(plan.channels.subtotal, 352_386_816);
        assert_eq!(plan.combined, 2_106_672_544);

        let diagnostic = render(plan);
        assert!(diagnostic.contains("1,673.0 MiB"));
        assert!(diagnostic.contains("336.1 MiB"));
        assert!(diagnostic.contains("1.96 GiB (2,009.1 MiB)"));
        assert!(diagnostic.contains("not a hard RSS ceiling"));
        assert!(diagnostic.contains("unaccounted"));
    }

    #[test]
    fn sampled_overrides_and_host_default_recompute_every_subtotal() {
        let overrides = HashMap::from([
            ("BLIT_EXT_MAX_TRANSIENT", 7),
            ("BLIT_EXT_MEMORY_MAX", 32 * 1024 * 1024),
            ("BLIT_EXT_MAX_VALIDATING", 1),
            ("BLIT_CHANNEL_MAX_LISTENERS", 3),
            ("BLIT_CHANNEL_MAX_CONNECTED", 2),
            ("BLIT_CHANNEL_BUFFER_MAX", 5 * 1024 * 1024),
        ]);
        let inputs = CapacityInputs::sample(true, true, 3, |name, default| {
            overrides.get(name).copied().unwrap_or(default)
        });
        assert_eq!(inputs.max_running, 2);
        let plan = calculate(inputs);
        assert_eq!(
            plan.extensions.terminal_records,
            7 * (TERMINAL_STATUS_BYTES + TERMINAL_EXIT_BYTES)
        );
        assert_eq!(plan.extensions.validation_inputs, 64 * MIB);
        assert_eq!(plan.extensions.network_validation_requests, 64 * MIB);
        assert_eq!(plan.channels.listener_metadata, 3 * CHANNEL_METADATA_BYTES);
        assert_eq!(plan.channels.connected_metadata, 4 * CHANNEL_METADATA_BYTES);
        assert_eq!(plan.channels.window_reservations, 4 * MIB);
        assert_eq!(plan.admitted_channel_pairs, 2);
        assert!(render(plan).contains("derived from 7 transients"));
    }

    #[test]
    fn pair_count_and_window_bytes_jointly_bound_channel_reservations() {
        let mut inputs = defaults(8);
        inputs.channel_pairs = 1;
        inputs.channel_buffer_bytes = 256 * 1024 * 1024;
        let count_limited = calculate(inputs);
        assert_eq!(count_limited.admitted_channel_pairs, 1);
        assert_eq!(count_limited.channels.window_reservations, 2 * MIB);

        inputs.channel_pairs = 128;
        inputs.channel_buffer_bytes = 5 * 1024 * 1024;
        let byte_limited = calculate(inputs);
        assert_eq!(byte_limited.admitted_channel_pairs, 2);
        assert_eq!(byte_limited.channels.window_reservations, 4 * MIB);
        assert_eq!(
            byte_limited.channels.connected_metadata,
            4 * CHANNEL_METADATA_BYTES
        );
    }

    #[test]
    fn disabled_families_contribute_zero_but_keep_derived_diagnostics() {
        let mut inputs = defaults(8);
        inputs.extensions_enabled = false;
        inputs.channels_enabled = false;
        inputs.max_transient = 9;
        let plan = calculate(inputs);
        assert_eq!(plan.extensions.subtotal, 0);
        assert_eq!(plan.channels.subtotal, 0);
        assert_eq!(plan.combined, 0);
        assert_eq!(
            plan.extensions.terminal_records,
            9 * (TERMINAL_STATUS_BYTES + TERMINAL_EXIT_BYTES)
        );
        let diagnostic = render(plan);
        assert_eq!(
            diagnostic.matches("disabled (active subtotal 0 B)").count(),
            2
        );
        assert!(diagnostic.contains("combined plan: 0 B"));
    }

    #[test]
    fn host_running_default_tracks_the_actual_sampled_cpu_count() {
        assert_eq!(defaults(0).max_running, 1);
        assert_eq!(defaults(1).max_running, 1);
        assert_eq!(defaults(2).max_running, 1);
        assert_eq!(defaults(3).max_running, 2);
        assert_eq!(defaults(5).max_running, 4);
        assert_eq!(defaults(64).max_running, 4);
    }

    #[test]
    fn one_call_startup_hook_samples_calculates_and_renders() {
        let diagnostic = sampled_diagnostic(true, true, 8, |_, default| default);
        assert!(diagnostic.starts_with("blit-server: startup capacity plan"));
        assert!(diagnostic.contains("running attempts 776.0 MiB (4 max)"));
        assert!(diagnostic.contains("combined plan: 1.96 GiB"));
    }
}
