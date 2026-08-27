# Orbit Option 2 design QA

## Evidence

- Reference: `C:\Users\kraji\.codex\generated_images\01a038e7-6365-70c1-8c55-8f66f94c110b\exec-e1ffba2a-7d18-46b3-965b-8e2d7327d9ff.png`
- Implementation: `assets/orbit-tui.png`
- Combined comparison: `C:\Users\kraji\AppData\Local\Temp\orbit-ui-audit\comparison.png`
- Terminal viewport: 120 columns by 40 rows
- Capture path: real Linux PTY via `script`, serialized with xterm-headless, rendered in Chrome at 1100 by 800 pixels

## Fidelity review

| Surface | Result |
| --- | --- |
| Typography | Monospace hierarchy, violet brand emphasis, bold conversation names, subdued metadata, and high-contrast message bodies match the reference. |
| Spacing and layout | Two-column inbox/thread composition, narrow header, date divider, persistent composer, command rail, and status rail match the selected structure. |
| Colors and tokens | Graphite background, violet selection/focus, mint presence/connectivity, blue-gray secondary text, and restrained borders match the selected palette. |
| Image and icon fidelity | The source contains no avatar or raster assets. Terminal-safe text markers are used only for functional status and keyboard hints. |
| Copy and content | Orbit branding replaces the concept name. Unsupported unread/archive/mute states and an unverified encryption promise are intentionally omitted. |
| Responsiveness | The reference state is verified at 120 by 40. Automated render coverage also exercises 60 by 18, 80 by 24, 96 by 28, and 160 by 50 across all five themes and all interaction modes. |
| Interaction and accessibility | Search, navigation, open, multiline compose, explicit send, privacy masking, help, theme selection, mouse targets, bounded paste, and terminal restoration are covered by automated tests and PTY exercise. Color-dependent states retain text labels. |

## Iterations

The first implementation comparison exposed a weak selected-row contrast, UTC-derived display time,
inline transcript timestamps, and a date divider that did not identify the current day. The final
pass introduced a dedicated selection token, local time formatting, right-aligned message times, and
the `Today` date label. The composer was captured in its focused state to verify the primary journey.

No unresolved P0, P1, or P2 visual defects remain in the selected viewport.

final result: passed
