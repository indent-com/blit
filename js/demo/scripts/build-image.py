#!/usr/bin/env python3
"""Build the demo sandbox image on Modal and print the image ID.

Usage:
    uv run python js/demo/scripts/build-image.py

Update DEMO_IMAGE_ID in js/demo/src/demo.ts with the printed image ID.
"""

import modal

PARROT_GIF_URL = "https://raw.githubusercontent.com/indent-com/assets/indent-2026-04-10-hires-spin/parrot_1k.gif"
BLIT_DEB_URL = "https://github.com/indent-com/blit/releases/download/v0.22.0/blit_0.22.0_amd64.deb"

image = (
    modal.Image.from_registry("ubuntu:24.04")
    .apt_install("bash", "curl", "git", "mpv", "ffmpeg", "ca-certificates")
    .run_commands(
        f"curl -fsSL {BLIT_DEB_URL} -o /tmp/blit.deb && dpkg -i /tmp/blit.deb && rm /tmp/blit.deb",
        "curl -fsSL https://opencode.ai/install | bash && ln -sf /root/.opencode/bin/opencode /usr/local/bin/opencode",
        "opencode providers list > /dev/null 2>&1 || true",
        f"mkdir -p /home/blit && curl -sfL -o /tmp/parrot_1k.gif '{PARROT_GIF_URL}'"
        " && ffmpeg -i /tmp/parrot_1k.gif -vf scale=160:80 /home/blit/parrot.gif && rm /tmp/parrot_1k.gif",
        "git config --global user.email demo@example.com",
        "git config --global user.name Demo",
        'mkdir -p /home/blit/project && cd /home/blit/project && git init && echo "# Project" > README.md && git add -A && git commit -m init',
    )
)

app = modal.App.lookup("blit-demo", create_if_missing=True)

with modal.enable_output():
    built = image.build(app)

print(f"\nDEMO_IMAGE_ID = \"{built.object_id}\"")
print("Update this value in js/demo/src/demo.ts")
