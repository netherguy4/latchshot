use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use log::warn;
use smithay_client_toolkit::dispatch2::Dispatch2;
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::{delegate_registry, registry_handlers};
use wayland_client::backend::ObjectData;
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::{Connection, Proxy, QueueHandle, WEnum};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::{
    self, ExtForeignToplevelHandleV1,
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_list_v1::{
    self, ExtForeignToplevelListV1,
};
use wayland_protocols::ext::workspace::v1::client::ext_workspace_group_handle_v1::ExtWorkspaceGroupHandleV1;
use wayland_protocols::ext::workspace::v1::client::ext_workspace_handle_v1::{
    self, ExtWorkspaceHandleV1,
};
use wayland_protocols::ext::workspace::v1::client::ext_workspace_manager_v1::{
    self, ExtWorkspaceManagerV1,
};

use self::protocol::toplevel_info::zcosmic_toplevel_handle_v1::{self, ZcosmicToplevelHandleV1};
use self::protocol::toplevel_info::zcosmic_toplevel_info_v1::{self, ZcosmicToplevelInfoV1};
use super::SceneReader;
use super::generic::Generic;
use crate::{Rect, Scene, Window};

/// Upper bound for cosmic-comp to report every toplevel's state.
const STATE_TIMEOUT: Duration = Duration::from_secs(1);
const STATE_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// [COSMIC](https://github.com/pop-os/cosmic-comp) window discovery through
/// `zcosmic_toplevel_info_v1` geometry and `ext-workspace-v1` visibility.
pub(super) struct Cosmic {
    outputs: Generic,
}

impl Cosmic {
    pub(super) fn connect() -> Result<Self> {
        Ok(Self {
            outputs: Generic::connect()?,
        })
    }
}

impl SceneReader for Cosmic {
    fn scene(&mut self) -> Result<Scene> {
        let mut scene = self.outputs.scene()?;
        // Region and output selection still work without window snapping.
        scene.windows = windows().unwrap_or_else(|error| {
            warn!("COSMIC window discovery failed: {error:#}");
            Vec::new()
        });

        Ok(scene)
    }
}

fn windows() -> Result<Vec<Window>> {
    let connection = Connection::connect_to_env()?;
    let (globals, mut event_queue) = registry_queue_init(&connection)?;
    let qh = event_queue.handle();
    let _list: ExtForeignToplevelListV1 = globals
        .bind(&qh, 1..=1, Data)
        .context("ext_foreign_toplevel_list_v1 is unavailable")?;
    // Version 3 reports workspaces through ext-workspace-v1.
    let info: ZcosmicToplevelInfoV1 = globals
        .bind(&qh, 3..=3, Data)
        .context("zcosmic_toplevel_info_v1 version 3 is unavailable")?;
    let _workspaces: ExtWorkspaceManagerV1 = globals
        .bind(&qh, 1..=1, Data)
        .context("ext_workspace_manager_v1 is unavailable")?;
    let mut state = State {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        toplevels: Vec::new(),
        active_workspaces: Vec::new(),
    };

    event_queue
        .roundtrip(&mut state)
        .context("failed to list COSMIC toplevels")?;
    // cosmic-comp does not reply to get_cosmic_toplevel directly: its refresh,
    // rate-limited to once per 150 ms, sends each handle's state together with
    // its geometry and workspaces. Toplevels mapped meanwhile need handles too.
    let deadline = Instant::now() + STATE_TIMEOUT;
    loop {
        for toplevel in &mut state.toplevels {
            if toplevel.cosmic.is_none() && !toplevel.info.closed {
                toplevel.cosmic = Some(info.get_cosmic_toplevel(&toplevel.handle, &qh, Data));
            }
        }
        event_queue
            .roundtrip(&mut state)
            .context("failed to read COSMIC toplevel state")?;
        let pending = state
            .toplevels
            .iter()
            .filter(|toplevel| !toplevel.info.closed && toplevel.info.states.is_none())
            .count();
        if pending == 0 {
            break;
        }
        if Instant::now() >= deadline {
            warn!("{pending} COSMIC toplevels did not report their state in time");
            break;
        }
        thread::sleep(STATE_POLL_INTERVAL);
    }

    let candidates = state
        .toplevels
        .iter()
        .filter_map(|toplevel| {
            let output = state.output_state.info(toplevel.output.as_ref()?)?;
            let on_active_workspace = toplevel
                .workspaces
                .iter()
                .any(|workspace| state.active_workspaces.contains(workspace));

            candidate(
                &toplevel.info,
                on_active_workspace,
                output.logical_position.unwrap_or(output.location),
            )
        })
        .collect();

    Ok(stack(candidates))
}

/// Protocol-independent toplevel state.
#[derive(Default)]
struct ToplevelInfo {
    identifier: Option<String>,
    /// Position relative to the output named by the geometry event, and size.
    geometry: Option<(i32, i32, i32, i32)>,
    /// `None` until cosmic-comp reports the state.
    states: Option<Vec<u32>>,
    closed: bool,
}

impl ToplevelInfo {
    fn has(&self, state: ToplevelState) -> bool {
        self.states
            .as_ref()
            .is_some_and(|states| states.contains(&(state as u32)))
    }
}

struct Candidate {
    window: Window,
    activated: bool,
    fullscreen: bool,
    sticky: bool,
}

/// Returns the selectable window for a toplevel, in global logical
/// coordinates, unless it is hidden.
fn candidate(
    info: &ToplevelInfo,
    on_active_workspace: bool,
    (output_x, output_y): (i32, i32),
) -> Option<Candidate> {
    let sticky = info.has(ToplevelState::Sticky);
    if info.closed || info.has(ToplevelState::Minimized) || !(sticky || on_active_workspace) {
        return None;
    }
    let (x, y, width, height) = info.geometry?;

    Some(Candidate {
        window: Window {
            geometry: Rect::new(
                f64::from(output_x + x),
                f64::from(output_y + y),
                f64::from(width),
                f64::from(height),
            ),
            identifier: info.identifier.clone(),
        },
        activated: info.has(ToplevelState::Activated),
        fullscreen: info.has(ToplevelState::Fullscreen),
        sticky,
    })
}

/// Orders windows from front to back.
///
/// COSMIC exposes no stacking order. cosmic-comp draws a focused fullscreen
/// window above everything, sticky windows above the workspace, and raises the
/// focused window; overlapping unfocused floating windows are guessed by size,
/// smaller in front.
fn stack(mut candidates: Vec<Candidate>) -> Vec<Window> {
    candidates.sort_by(|a, b| {
        let rank = |c: &Candidate| {
            (
                c.activated && c.fullscreen,
                c.sticky,
                c.activated,
                c.fullscreen,
            )
        };
        rank(b)
            .cmp(&rank(a))
            .then_with(|| area(&a.window).total_cmp(&area(&b.window)))
    });

    candidates.into_iter().map(|c| c.window).collect()
}

fn area(window: &Window) -> f64 {
    window.geometry.width() * window.geometry.height()
}

type ToplevelState = zcosmic_toplevel_handle_v1::State;

struct Toplevel {
    handle: ExtForeignToplevelHandleV1,
    cosmic: Option<ZcosmicToplevelHandleV1>,
    output: Option<WlOutput>,
    workspaces: Vec<ExtWorkspaceHandleV1>,
    info: ToplevelInfo,
}

struct State {
    registry_state: RegistryState,
    output_state: OutputState,
    toplevels: Vec<Toplevel>,
    active_workspaces: Vec<ExtWorkspaceHandleV1>,
}

/// Object data shared by every protocol object this backend creates.
struct Data;

impl Dispatch2<ExtForeignToplevelListV1, State> for Data {
    fn event(
        &self,
        state: &mut State,
        _: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            state.toplevels.push(Toplevel {
                handle: toplevel,
                cosmic: None,
                output: None,
                workspaces: Vec::new(),
                info: ToplevelInfo::default(),
            });
        }
    }

    fn event_created_child(_: u16, qh: &QueueHandle<State>) -> Arc<dyn ObjectData> {
        qh.make_data::<ExtForeignToplevelHandleV1, _>(Self)
    }
}

impl Dispatch2<ExtForeignToplevelHandleV1, State> for Data {
    fn event(
        &self,
        state: &mut State,
        handle: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        let Some(toplevel) = state.toplevels.iter_mut().find(|t| t.handle == *handle) else {
            return;
        };
        match event {
            ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } => {
                toplevel.info.identifier = Some(identifier);
            }
            ext_foreign_toplevel_handle_v1::Event::Closed => toplevel.info.closed = true,
            _ => {}
        }
    }
}

impl Dispatch2<ZcosmicToplevelInfoV1, State> for Data {
    fn event(
        &self,
        _: &mut State,
        _: &ZcosmicToplevelInfoV1,
        _: zcosmic_toplevel_info_v1::Event,
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
    }
}

impl Dispatch2<ZcosmicToplevelHandleV1, State> for Data {
    fn event(
        &self,
        state: &mut State,
        handle: &ZcosmicToplevelHandleV1,
        event: zcosmic_toplevel_handle_v1::Event,
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        let Some(toplevel) = state
            .toplevels
            .iter_mut()
            .find(|t| t.cosmic.as_ref() == Some(handle))
        else {
            return;
        };
        match event {
            zcosmic_toplevel_handle_v1::Event::Geometry {
                output,
                x,
                y,
                width,
                height,
            } => {
                toplevel.output = Some(output);
                toplevel.info.geometry = Some((x, y, width, height));
            }
            zcosmic_toplevel_handle_v1::Event::State { state } => {
                toplevel.info.states = Some(
                    state
                        .as_chunks()
                        .0
                        .iter()
                        .map(|bytes| u32::from_ne_bytes(*bytes))
                        .collect(),
                );
            }
            zcosmic_toplevel_handle_v1::Event::ExtWorkspaceEnter { workspace } => {
                toplevel.workspaces.push(workspace);
            }
            zcosmic_toplevel_handle_v1::Event::ExtWorkspaceLeave { workspace } => {
                toplevel.workspaces.retain(|w| *w != workspace);
            }
            _ => {}
        }
    }
}

impl Dispatch2<ExtWorkspaceManagerV1, State> for Data {
    fn event(
        &self,
        _: &mut State,
        _: &ExtWorkspaceManagerV1,
        _: ext_workspace_manager_v1::Event,
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
    }

    fn event_created_child(opcode: u16, qh: &QueueHandle<State>) -> Arc<dyn ObjectData> {
        if opcode == ext_workspace_manager_v1::EVT_WORKSPACE_GROUP_OPCODE {
            qh.make_data::<ExtWorkspaceGroupHandleV1, _>(Self)
        } else {
            qh.make_data::<ExtWorkspaceHandleV1, _>(Self)
        }
    }
}

impl Dispatch2<ExtWorkspaceGroupHandleV1, State> for Data {
    fn event(
        &self,
        _: &mut State,
        _: &ExtWorkspaceGroupHandleV1,
        _: <ExtWorkspaceGroupHandleV1 as Proxy>::Event,
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
    }
}

impl Dispatch2<ExtWorkspaceHandleV1, State> for Data {
    fn event(
        &self,
        state: &mut State,
        workspace: &ExtWorkspaceHandleV1,
        event: ext_workspace_handle_v1::Event,
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        if let ext_workspace_handle_v1::Event::State {
            state: WEnum::Value(flags),
        } = event
        {
            state.active_workspaces.retain(|w| w != workspace);
            if flags.contains(ext_workspace_handle_v1::State::Active) {
                state.active_workspaces.push(workspace.clone());
            }
        }
    }
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
}

delegate_registry!(State);

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState];
}

smithay_client_toolkit::delegate_dispatch2!(State);

/// Bindings generated from protocol files vendored unmodified from
/// [pop-os/cosmic-protocols@32283d7](https://github.com/pop-os/cosmic-protocols/tree/32283d76a8d0342da74c4cc022a533c52dcf378f/unstable).
/// The XML files carry a permissive license, unlike the GPL-3.0-only crate.
/// cosmic-workspace-unstable-v1 is only needed because the deprecated
/// toplevel workspace events reference it.
#[allow(
    dead_code,
    missing_docs,
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    unused_imports,
    unused_variables,
    clippy::all,
    clippy::pedantic,
    clippy::nursery
)]
mod protocol {
    pub mod workspace {
        use wayland_client;
        use wayland_client::protocol::*;

        pub mod __interfaces {
            use wayland_client::backend as wayland_backend;
            use wayland_client::protocol::__interfaces::*;
            wayland_scanner::generate_interfaces!("protocols/cosmic-workspace-unstable-v1.xml");
        }
        use self::__interfaces::*;

        wayland_scanner::generate_client_code!("protocols/cosmic-workspace-unstable-v1.xml");
    }

    pub mod toplevel_info {
        use wayland_client;
        use wayland_client::protocol::*;
        use wayland_protocols::ext::foreign_toplevel_list::v1::client::*;
        use wayland_protocols::ext::workspace::v1::client::*;

        use super::workspace::*;

        pub mod __interfaces {
            use wayland_client::backend as wayland_backend;
            use wayland_client::protocol::__interfaces::*;
            use wayland_protocols::ext::foreign_toplevel_list::v1::client::__interfaces::*;
            use wayland_protocols::ext::workspace::v1::client::__interfaces::*;

            use super::super::workspace::__interfaces::*;
            wayland_scanner::generate_interfaces!("protocols/cosmic-toplevel-info-unstable-v1.xml");
        }
        use self::__interfaces::*;

        wayland_scanner::generate_client_code!("protocols/cosmic-toplevel-info-unstable-v1.xml");
    }
}

#[cfg(test)]
mod tests {
    use super::{Candidate, ToplevelInfo, ToplevelState, candidate, stack};
    use crate::{Rect, Window};

    fn info(states: &[ToplevelState]) -> ToplevelInfo {
        ToplevelInfo {
            identifier: Some("toplevel".to_owned()),
            geometry: Some((10, 20, 300, 200)),
            states: Some(states.iter().map(|state| *state as u32).collect()),
            closed: false,
        }
    }

    #[test]
    fn output_local_geometry_becomes_global() {
        let candidate = candidate(&info(&[ToplevelState::Activated]), true, (1920, -40)).unwrap();

        assert_eq!(
            candidate.window.geometry,
            Rect::new(1930.0, -20.0, 300.0, 200.0)
        );
        assert_eq!(candidate.window.identifier.as_deref(), Some("toplevel"));
        assert!(candidate.activated);
    }

    #[test]
    fn hidden_toplevels_are_not_selectable() {
        let minimized = info(&[ToplevelState::Minimized]);
        let closed = ToplevelInfo {
            closed: true,
            ..info(&[])
        };
        let unmapped = ToplevelInfo {
            geometry: None,
            ..info(&[])
        };

        assert!(candidate(&minimized, true, (0, 0)).is_none());
        assert!(candidate(&closed, true, (0, 0)).is_none());
        assert!(candidate(&unmapped, true, (0, 0)).is_none());
        assert!(candidate(&info(&[]), false, (0, 0)).is_none());
    }

    #[test]
    fn sticky_toplevels_ignore_workspaces() {
        assert!(candidate(&info(&[ToplevelState::Sticky]), false, (0, 0)).is_some());
    }

    fn window(name: &str, size: f64, activated: bool, fullscreen: bool) -> Candidate {
        Candidate {
            window: Window {
                geometry: Rect::new(0.0, 0.0, size, size),
                identifier: Some(name.to_owned()),
            },
            activated,
            fullscreen,
            sticky: false,
        }
    }

    #[test]
    fn focused_fullscreen_then_focused_then_smaller_windows_come_first() {
        let order = stack(vec![
            window("large", 900.0, false, false),
            window("fullscreen", 1000.0, false, true),
            window("small", 100.0, false, false),
            window("focused", 500.0, true, false),
        ])
        .into_iter()
        .map(|window| window.identifier.unwrap())
        .collect::<Vec<_>>();

        assert_eq!(order, ["focused", "fullscreen", "small", "large"]);
    }
}
