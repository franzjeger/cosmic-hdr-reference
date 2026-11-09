# AMD HDR bring-up notes

Validated on 2026-09-02 with an AMD Rembrandt Radeon 680M and an LG HDR
television connected as `HDMI-A-1`, running Arch Linux and COSMIC 1.7.

## Symptoms

The greeter accepted the login, but the HDR session remained black. The
compositor process stayed alive rather than returning a useful startup error.

## Root causes

1. CTA-861 permits the desired-content luminance bytes in the HDR Static
   Metadata Data Block to be absent. The affected display advertises Static
   Metadata Type 1, PQ, and BT.2020 correctly, but reports zero for the optional
   desired-content maximum luminance. Treating zero as "not HDR capable"
   rejected a valid sink. The prototype now uses a conservative 1000 cd/m2
   internal ceiling when the optional value is absent.
2. A strict HDR startup error was returned while `KmsGuard` and a compositor
   write guard were still live. Dropping the partially initialized compositor
   then dropped `DrmOutput`, which re-entered KMS configuration and attempted
   to acquire the same lock. The render thread self-deadlocked and produced a
   permanent black session.

## Fix

`patches/cosmic-comp-amd-hdr-startup.patch` contains the focused compositor and
Smithay changes. `LockedDrmOutputManager::remove_output` removes the CRTC's
`DrmCompositor` directly through the already-held write guard and marks the
`DrmOutput` as removed so its `Drop` implementation does not re-lock the
compositor map. The strict failure paths release `compositor_ref` and call
`drm.remove_output(compositor)` before returning the error.

The session watchdog was also changed to fail closed: failed live HDR
validation requests a clean compositor shutdown and terminates the logind
session if the compositor does not exit within ten seconds.

## Scope

This confirms the output path on one AMD APU and one HDMI television. It does
not establish support for every AMD display engine, connector, or sink.
