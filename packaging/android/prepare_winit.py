#!/usr/bin/env python3
"""Apply Android hover support to an isolated copy of the locked, published winit crate.

The desktop dependency and the checkout's Cargo.lock stay unchanged. Android packaging uses
the copied workspace and crate, with the same dependency versions apart from winit's source.
Remove this bridge when the winit version used by eframe emits Android hover events itself.
"""

import json
import os
import pathlib
import shutil
import subprocess
import sys
import tomllib

repo = pathlib.Path(__file__).resolve().parents[2]
stage = pathlib.Path(sys.argv[1]).resolve()
metadata = json.loads(subprocess.check_output(["cargo", "metadata", "--locked", "--format-version", "1"], cwd=repo))
package = next(p for p in metadata["packages"] if p["name"] == "winit" and p["version"] == "0.30.13")
source = pathlib.Path(package["manifest_path"]).parent
patched = stage / "winit-hover"
workspace = stage / "workspace"
old_module = patched / "src/platform_impl/android/mod.rs"
previous = (old_module.read_bytes(), old_module.stat().st_atime_ns, old_module.stat().st_mtime_ns) if old_module.is_file() else None
# Refresh the same pinned crate in place; interrupted packaging can leave a partial tree.
# Its version is checked above, so there are no files from another winit release to retain.
shutil.copytree(source, patched, dirs_exist_ok=True)
module = patched / "src/platform_impl/android/mod.rs"
text = module.read_text()
marker = "                let phase = match motion_event.action() {"
if text.count(marker) != 1:
    raise SystemExit("The locked winit input code changed; review the Android hover bridge.")
hover = """                // Android pens report hover separately from touch. Emit pointer movement
                // without synthesizing a button press, so hovering cannot draft a point.
                match motion_event.action() {
                    MotionAction::HoverEnter | MotionAction::HoverMove => {
                        if let Some(pointer) = motion_event.pointers().next() {
                            let (x, y) = (pointer.x(), pointer.y());
                            if x.is_finite() && y.is_finite() {
                                if motion_event.action() == MotionAction::HoverEnter {
                                    callback(event::Event::WindowEvent {
                                        window_id,
                                        event: event::WindowEvent::CursorEntered { device_id },
                                    }, self.window_target());
                                }
                                callback(event::Event::WindowEvent {
                                    window_id,
                                    event: event::WindowEvent::CursorMoved {
                                        device_id,
                                        position: PhysicalPosition { x: x as f64, y: y as f64 },
                                    },
                                }, self.window_target());
                            }
                        }
                        return input_status;
                    },
                    MotionAction::HoverExit => {
                        callback(event::Event::WindowEvent {
                            window_id,
                            event: event::WindowEvent::CursorLeft { device_id },
                        }, self.window_target());
                        return input_status;
                    },
                    _ => {},
                }

"""
pen = """                // Preserve pen identity: pen contact is a pointer, fingers remain Touch.
                let is_pen = |tool| matches!(tool, android_activity::input::ToolType::Stylus | android_activity::input::ToolType::Eraser);
                let selected = motion_event.pointer_at_index(motion_event.pointer_index());
                let selected_pen = is_pen(selected.tool_type());
                let action = motion_event.action();
                let pen = motion_event.pointers().find(|p| is_pen(p.tool_type()));
                if let Some(pointer) = pen {
                    let pen_action = match action {
                        MotionAction::Down | MotionAction::PointerDown | MotionAction::Up | MotionAction::PointerUp => selected_pen,
                        MotionAction::Move | MotionAction::Cancel | MotionAction::HoverEnter | MotionAction::HoverMove | MotionAction::HoverExit
                        | MotionAction::ButtonPress | MotionAction::ButtonRelease => true,
                        _ => false,
                    };
                    if pen_action {
                        let (x, y) = (pointer.x(), pointer.y());
                        if !x.is_finite() || !y.is_finite() { return input_status; }
                        if action == MotionAction::HoverEnter {
                            callback(event::Event::WindowEvent { window_id, event: event::WindowEvent::CursorEntered { device_id } }, self.window_target());
                        }
                        if action != MotionAction::HoverExit {
                            callback(event::Event::WindowEvent { window_id, event: event::WindowEvent::CursorMoved {
                                device_id, position: PhysicalPosition { x: x as f64, y: y as f64 },
                            } }, self.window_target());
                        }
                        // API 33 FLAG_CANCELED (0x20) marks a rejected UP; release cannot place a point.
                        let cancelled = action == MotionAction::Cancel || (matches!(action, MotionAction::Up | MotionAction::PointerUp) && motion_event.flags().0 & 0x20 != 0);
                        if cancelled {
                            for state in [event::ElementState::Pressed, event::ElementState::Released] {
                                callback(event::Event::WindowEvent { window_id, event: event::WindowEvent::KeyboardInput {
                                    device_id, is_synthetic: true, event: event::KeyEvent {
                                        state, physical_key: crate::keyboard::PhysicalKey::Code(crate::keyboard::KeyCode::Escape),
                                        logical_key: crate::keyboard::Key::Named(crate::keyboard::NamedKey::Escape),
                                        location: crate::keyboard::KeyLocation::Standard, repeat: false, text: None, platform_specific: KeyEventExtra {},
                                    },
                                } }, self.window_target());
                            }
                        }
                        let state = match action {
                            MotionAction::Down | MotionAction::PointerDown => Some(event::ElementState::Pressed),
                            MotionAction::Up | MotionAction::PointerUp | MotionAction::Cancel => Some(event::ElementState::Released),
                            _ => None,
                        };
                        if let Some(state) = state {
                            callback(event::Event::WindowEvent { window_id, event: event::WindowEvent::MouseInput { device_id, state, button: event::MouseButton::Left } }, self.window_target());
                        }
                        if matches!(action, MotionAction::ButtonPress | MotionAction::ButtonRelease) {
                            let buttons = motion_event.button_state();
                            if action == MotionAction::ButtonRelease || buttons.stylus_primary() || buttons.stylus_secondary() {
                                let state = if action == MotionAction::ButtonPress { event::ElementState::Pressed } else { event::ElementState::Released };
                                callback(event::Event::WindowEvent { window_id, event: event::WindowEvent::MouseInput { device_id, state, button: event::MouseButton::Back } }, self.window_target());
                            }
                        }
                        if cancelled || action == MotionAction::HoverExit {
                            callback(event::Event::WindowEvent { window_id, event: event::WindowEvent::CursorLeft { device_id } }, self.window_target());
                        }
                        if !matches!(action, MotionAction::Move | MotionAction::Cancel) { return input_status; }
                    }
                }

"""
text = text.replace(marker, pen + hover + marker)
loop_marker = "                    for pointer in pointers {"
if text.count(loop_marker) != 1:
    raise SystemExit("The locked winit touch loop changed; review the pen bridge.")
text = text.replace(loop_marker, loop_marker + """
                        if is_pen(pointer.tool_type()) { continue; }
                        let phase = if pointer.tool_type() == android_activity::input::ToolType::Palm
                            || (matches!(motion_event.action(), MotionAction::Up | MotionAction::PointerUp) && motion_event.flags().0 & 0x20 != 0) {
                            event::TouchPhase::Cancelled
                        } else { phase };
""")
module.write_text(text)
# An identical pinned bridge should not rebuild winit and all UI dependencies.
if previous and previous[0] == text.encode():
    os.utime(module, ns=(previous[1], previous[2]))
paths = subprocess.check_output(["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"], cwd=repo).decode().split("\0")
workspace.mkdir(parents=True, exist_ok=True)
for name in paths:
    if not name or name.startswith(("dist/", "target/", "plan/")):
        continue
    src = repo / name
    if src.is_file():
        dest = workspace / name
        dest.parent.mkdir(parents=True, exist_ok=True)
        # Keep unchanged inputs stable, but give changed contents the copy's timestamp.
        # Restoring an older source mtime can otherwise make Cargo reuse stale object code
        # when a build snapshot was taken before an edit during an earlier compilation.
        if not dest.is_file() or src.read_bytes() != dest.read_bytes():
            shutil.copy(src, dest)
config = workspace / ".cargo/config.toml"
config.parent.mkdir(parents=True, exist_ok=True)
original = (repo / ".cargo/config.toml").read_text() if (repo / ".cargo/config.toml").exists() else ""
if "[patch.crates-io]" in original:
    raise SystemExit("Review the existing Cargo patch before applying the Android hover bridge.")
config.write_text(original + '\n[patch.crates-io]\nwinit = { path = ' + json.dumps(str(patched)) + ' }\n')
subprocess.run(["cargo", "metadata", "--offline", "--format-version", "1"], cwd=workspace, check=True, stdout=subprocess.DEVNULL)
before = tomllib.loads((repo / "Cargo.lock").read_text())
after = tomllib.loads((workspace / "Cargo.lock").read_text())
normalize = lambda lock: [p for p in lock["package"] if p["name"] != "winit"]
if normalize(before) != normalize(after):
    raise SystemExit("Android hover patch changed another dependency; refusing an unpinned build.")
print(workspace)
