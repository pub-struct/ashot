//! Monitor enumeration via wl_output + zxdg_output_v1.
//!
//! Gives each monitor's logical position/size (compositor layout space) and
//! physical pixel mode, which lets us map monitors onto the portal's
//! full-desktop screenshot.

use wayland_client::{
    protocol::{wl_output, wl_registry},
    Connection, Dispatch, Proxy, QueueHandle,
};
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1, zxdg_output_v1::ZxdgOutputV1,
};

use crate::error::{Error, Result};

#[derive(Debug, Clone, serde::Serialize)]
pub struct Monitor {
    pub index: usize,
    pub name: String,
    /// Position and size in the compositor's logical layout space.
    pub logical_x: i32,
    pub logical_y: i32,
    pub logical_w: i32,
    pub logical_h: i32,
    /// Current mode in physical pixels.
    pub pixel_w: i32,
    pub pixel_h: i32,
    pub scale: i32,
}

#[derive(Default)]
struct OutputData {
    name: Option<String>,
    logical_pos: Option<(i32, i32)>,
    logical_size: Option<(i32, i32)>,
    mode: Option<(i32, i32)>,
    scale: i32,
}

#[derive(Default)]
struct State {
    wl_outputs: Vec<wl_output::WlOutput>,
    xdg_manager: Option<ZxdgOutputManagerV1>,
    outputs: Vec<OutputData>,
}

pub fn enumerate() -> Result<Vec<Monitor>> {
    let conn = Connection::connect_to_env()
        .map_err(|e| Error::WaylandUnavailable(e.to_string()))?;
    let display = conn.display();
    let mut queue = conn.new_event_queue::<State>();
    let qh = queue.handle();
    display.get_registry(&qh, ());

    let mut state = State::default();
    // Round 1: learn globals, bind wl_outputs + xdg output manager.
    queue
        .roundtrip(&mut state)
        .map_err(|e| Error::WaylandUnavailable(e.to_string()))?;
    // Round 2: wl_output events (mode/scale/name) arrive after binding.
    queue
        .roundtrip(&mut state)
        .map_err(|e| Error::WaylandUnavailable(e.to_string()))?;

    if let Some(manager) = &state.xdg_manager {
        let outputs = state.wl_outputs.clone();
        for (i, output) in outputs.iter().enumerate() {
            manager.get_xdg_output(output, &qh, i);
        }
        queue
            .roundtrip(&mut state)
            .map_err(|e| Error::WaylandUnavailable(e.to_string()))?;
    }

    let monitors: Vec<Monitor> = state
        .outputs
        .iter()
        .enumerate()
        .filter_map(|(i, o)| {
            let (lx, ly) = o.logical_pos?;
            let (lw, lh) = o.logical_size?;
            let (pw, ph) = o.mode.unwrap_or((lw, lh));
            Some(Monitor {
                index: i,
                name: o.name.clone().unwrap_or_else(|| format!("output-{i}")),
                logical_x: lx,
                logical_y: ly,
                logical_w: lw,
                logical_h: lh,
                pixel_w: pw,
                pixel_h: ph,
                scale: o.scale.max(1),
            })
        })
        .collect();

    if monitors.is_empty() {
        return Err(Error::WaylandUnavailable(
            "no usable outputs reported by the compositor".into(),
        ));
    }
    Ok(monitors)
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            if interface == wl_output::WlOutput::interface().name {
                let index = state.wl_outputs.len();
                let output =
                    registry.bind::<wl_output::WlOutput, _, _>(name, version.min(4), qh, index);
                state.wl_outputs.push(output);
                state.outputs.push(OutputData::default());
            } else if interface == ZxdgOutputManagerV1::interface().name {
                state.xdg_manager =
                    Some(registry.bind::<ZxdgOutputManagerV1, _, _>(name, version.min(3), qh, ()));
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, usize> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(data) = state.outputs.get_mut(*index) else { return };
        match event {
            wl_output::Event::Mode { flags, width, height, .. } => {
                if flags
                    .into_result()
                    .map(|f| f.contains(wl_output::Mode::Current))
                    .unwrap_or(false)
                {
                    data.mode = Some((width, height));
                }
            }
            wl_output::Event::Scale { factor } => data.scale = factor,
            wl_output::Event::Name { name } => data.name = Some(name),
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputV1, usize> for State {
    fn event(
        state: &mut Self,
        _: &ZxdgOutputV1,
        event: wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_v1::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_v1::Event;
        let Some(data) = state.outputs.get_mut(*index) else { return };
        match event {
            Event::LogicalPosition { x, y } => data.logical_pos = Some((x, y)),
            Event::LogicalSize { width, height } => data.logical_size = Some((width, height)),
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZxdgOutputManagerV1,
        _: <ZxdgOutputManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
