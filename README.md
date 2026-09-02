# COSMIC HDR reference implementation

Hardware-validated reference implementation of HDR output and HDR client
presentation for the COSMIC compositor and its Smithay backend.

This repository is a working integration snapshot, not an upstream-ready pull
request. It is published so COSMIC and Smithay developers can inspect, test,
and reuse the implementation while it is split into reviewable changes.

## What works

- Per-output HDR10 enablement with atomic KMS `Colorspace`,
  `HDR_OUTPUT_METADATA`, and 10-bit scanout state.
- SDR desktop composition mapped to BT.2020/PQ with adjustable per-output
  reference white.
- Live HDR and brightness changes through compositor configuration; the
  corresponding COSMIC Settings change is in
  `patches/cosmic-settings-hdr-brightness.patch`.
- Wayland color-management image descriptions and HDR client detection.
- Fullscreen/borderless Windows HDR client presentation using the scRGB
  reference-white convention used by Wine/Proton.
- Tearing-control protocol support, asynchronous KMS page flips, and an
  uncapped presentation path for clients that request tearing.
- Live VRR mode propagation. `Automatic` is the recommended desktop policy;
  forcing VRR continuously can expose normal panel luminance flicker as frame
  cadence changes.
- Clean connector color-state teardown and defensive KMS-state recovery.
- Stable HDR post-process element identity, preventing cursor movement from
  invalidating the whole output on every frame.

The COSMIC workspace currently passes all 20 unit and integration tests.

## Validated hardware

Validated on 2026-08-31 with:

- NVIDIA GeForce RTX 5090 (GB202)
- NVIDIA open kernel modules / driver 610.57.04
- Linux 7.2.1
- Two simultaneous HDR displays:
  - 3440x1440 up to 240 Hz
  - 3840x2160 up to 240 Hz

AMD validation was completed on 2026-09-02 with a Rembrandt Radeon 680M and
an LG HDR television over HDMI. The bring-up exposed an optional CTA luminance
handling issue and a strict-startup teardown deadlock. The focused fix and
analysis are in `patches/cosmic-comp-amd-hdr-startup.patch` and
`docs/amd-hdr-bringup.md`. Intel remains unvalidated.

## Current architecture and limitations

The compositor renders the ordinary SDR desktop into an 8-bit intermediate,
then performs the final SDR-to-BT.2020/PQ conversion into a 10-bit scanout
buffer. Native HDR client content can bypass that SDR mapping when it covers
the output and carries a compatible image description.

This proves the KMS, protocol, configuration, client-passthrough, and UX paths,
but it is not the desired long-term color architecture. A production design
should composite all surfaces in a linear, high-precision intermediate (for
example F16), apply per-surface color transforms and tone mapping there, and
encode only at the output boundary.

Direct and overlay scanout are disabled while the desktop HDR post-process is
active because those planes would bypass the shader. HDR client passthrough
can re-enable eligible primary-plane scanout.

## Layout

- `cosmic-comp/` - COSMIC compositor integration, configuration, rendering,
  HDR client handling, VRR, and tearing presentation.
- `smithay/` - atomic DRM color state, Wayland color-management support, HDR
  image descriptions, renderer hooks, and asynchronous page-flip support.
- `patches/` - companion COSMIC Settings patch.
- `docs/amd-hdr-bringup.md` - AMD failure analysis, validated hardware, and
  upstreaming guidance.

Imported source revisions recorded by the integration project:

- cosmic-comp: `c20b1d400efd2e090b6d7a63bf4e3ee718d65711`
- smithay: `360c2e35d87e4449e66c46fd2b309eda81e697a3`

## Build and test

```sh
cd cosmic-comp
cargo test --workspace
cargo build --release
```

The COSMIC workspace uses the included Smithay tree through a local path
dependency, so both sides of the integration are built together.

## Upstreaming

The intended next step is to split this snapshot into focused COSMIC and
Smithay pull requests. The highest-value seams are:

1. Typed atomic connector HDR state and test/commit consistency.
2. Wayland color-management protocol and surface-state plumbing.
3. Per-output COSMIC HDR configuration and capability reporting.
4. HDR composition and compatible client passthrough.
5. Tearing-control and asynchronous page-flip plumbing.

Code retains the licenses of the included upstream projects.
