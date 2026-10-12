# Matching Skyrim and Mudcrab camera captures

Mudcrab's `--shots` files specify absolute Creation-unit camera positions, yaw, pitch, horizontal FOV, and pixel dimensions. The Riverwood fixture is `crates/engine/tests/fixtures/color-grade-riverwood-shots.json`. `scripts/capture_color_grade.py` captures the same three poses with grading off, saturation only, and the full experimental grade. It rejects unsettled captures, mismatched poses or dimensions, and failed engine runs. Water animation is fixed at 1.0 seconds for these static shots.

## Recommended Skyrim camera framework

[FreeCamera Framework](https://www.nexusmods.com/skyrimspecialedition/mods/174046) has [public source and documentation](https://github.com/staalo18/FreeCameraFramework). Its [timeline format](https://github.com/staalo18/FreeCameraFramework/blob/main/Documentation/TimelineFileExample/TIMELINE_FORMAT.md) accepts YAML world-coordinate translation, world rotation `[pitch, roll, yaw]`, and FOV points. Set `followGround: false` to prevent automatic changes to the requested Z coordinate. Set `allowUserRotation: false`, `useDegrees: true`, and `playbackMode: wait` to hold the final pose.

Source inspection confirms that `TimelineManager::PlayTimeline` writes the timeline position to the free-camera translation, pitch/yaw to its rotation, and FOV to `PlayerCamera::worldFOV`. This establishes a declarative input path. It does not establish pixel-identical projection between Skyrim and Bevy. The framework also needs a client mod or its example to import and play a timeline; it is not a standalone photo-mode interface.

A draft for the terrace pose is:

```yaml
formatVersion: 1
playbackMode: wait
showMenusDuringPlayback: false
allowUserRotation: false
followGround: false
useDegrees: true
translationPoints:
  - time: 0.0
    type: world
    position: [23400.0, -47100.0, 400.0]
rotationPoints:
  - time: 0.0
    type: world
    rotation: [7.0, 0.0, 315.0]
fovPoints:
  - time: 0.0
    fov: 75.0
```

These angle and FOV values are calibration candidates, not verified native mappings. Before treating the resulting screenshots as references, check heading zero, yaw direction, pitch sign, camera position readback, and Skyrim's FOV convention at the chosen aspect ratio. Capture from the camera's actual eye, rather than assuming the player's position equals it.

Load the correct worldspace/cell separately and allow streaming and LOD to finish. Match weather, time of day, resolution, UI visibility, and the retail rendering configuration. The framework's verified camera input path does not automate these controls or screenshot saving. Its documented dependencies include SKSE64, Address Library, and powerofthree's Tweaks; verify runtime compatibility against the installed Skyrim version before deployment.

## Other candidates

| Tool | Useful role | Current limit |
| --- | --- | --- |
| [AutoTest](https://www.nexusmods.com/skyrimspecialedition/mods/42520), [source](https://github.com/Muriel-Salvan/AutoTest) | JSON test suites, location traversal, startup/exit automation | Default worldspace/grid or cell locations and camera panning do not specify an exact eye pose. A custom camera test adapter is needed. |
| [Photo Mode](https://www.nexusmods.com/skyrimspecialedition/mods/91778), [source](https://github.com/powerof3/PhotoMode) | Manual free-camera framing and screenshot workflow | No declarative exact-pose interface was established in this review. |
| [Screenshot Location and Metadata](https://github.com/maxkagamine/ScreenshotLocationAndMetadata) | Candidate metadata recorder for capture readback | Recording metadata does not establish camera input control. Verify the fields and runtime compatibility before relying on it. |
| [Camera Scripter](https://www.nexusmods.com/skyrim/mods/79041) | Legacy `.scs` camera scripting | A 32-bit Skyrim mod; the author's SE port restriction makes it unsuitable for this SE workflow. |

The present evidence consists of public documentation/source and Mudcrab captures. No Skyrim mod was installed or run, and no matched retail capture or retail color-grading parity was verified.
