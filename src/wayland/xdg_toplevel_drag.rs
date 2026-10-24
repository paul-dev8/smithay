//! XDG Toplevel Drag v1
//!
//! athena local change: this module is ours, not upstream smithay's (Chrome tab
//! tear-off needs it); see athena-compositor/smithay-local.diff.
//!
//! This protocol allows clients to move a window during a drag-and-drop operation,
//! enabling detachable window parts (e.g. Chrome tab tear-off).
//!
//! The compositor advertises the `xdg_toplevel_drag_manager_v1` global. Clients
//! create an `xdg_toplevel_drag_v1` object for a `wl_data_source` before starting
//! a drag. When the client wants to attach a toplevel to the drag, it calls `attach`
//! with x/y offsets. The compositor then positions the toplevel at
//! `cursor_position - offset` during the drag, as if `xdg_toplevel.move` were active.
//!
//! On `dnd_drop_performed`, the window's final position is wherever the cursor left it.
//!
//! Dispatch goes through [`Dispatch2`]/[`GlobalDispatch2`], so `delegate_dispatch2!`
//! covers this protocol; there is no delegate macro of its own.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use wayland_protocols::xdg::shell::server::xdg_toplevel::XdgToplevel;
use wayland_protocols::xdg::toplevel_drag::v1::server::{
    xdg_toplevel_drag_manager_v1::{self, XdgToplevelDragManagerV1},
    xdg_toplevel_drag_v1::{self, XdgToplevelDragV1},
};

use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, backend::GlobalId,
    protocol::wl_data_source::WlDataSource,
};

use wayland_server::protocol::wl_surface::WlSurface;

use crate::wayland::selection::data_device::DataSourceUserData;
use crate::wayland::shell::xdg::XdgShellSurfaceUserData;
use crate::wayland::{Dispatch2, GlobalData, GlobalDispatch2};

/// Per-drag state: which toplevel is attached and at what offset.
#[derive(Debug, Clone, Default)]
pub struct ToplevelDragState {
    /// The attached xdg_toplevel (if any)
    pub toplevel: Option<XdgToplevel>,
    /// The wl_surface underlying the attached toplevel
    pub wl_surface: Option<WlSurface>,
    /// Offset hint: the toplevel should be positioned at cursor - offset
    pub x_offset: i32,
    /// Offset hint, vertical: the toplevel should be positioned at cursor - offset
    pub y_offset: i32,
    /// Whether the drag has ended (drop performed or cancelled)
    pub drag_ended: bool,
}

/// Shared state for a single xdg_toplevel_drag_v1 object.
pub type ToplevelDragData = Arc<Mutex<ToplevelDragState>>;

/// User data associated with the xdg_toplevel_drag_v1 resource.
#[derive(Debug)]
pub struct XdgToplevelDragUserData {
    /// The drag's shared state, also handed to [`XdgToplevelDragHandler::toplevel_drag_attach`]
    pub state: ToplevelDragData,
    /// The data source this toplevel drag belongs to
    pub data_source: WlDataSource,
}

/// Handler trait for the xdg-toplevel-drag-v1 protocol.
///
/// Compositors implement this to be notified when a toplevel is attached to a drag.
pub trait XdgToplevelDragHandler: 'static {
    /// Called when a toplevel is attached to a drag operation.
    ///
    /// The compositor should start positioning the toplevel at `cursor - (x_offset, y_offset)`
    /// during the drag, and store a reference to `drag_state` so the DnD grab's motion
    /// handler can read it.
    fn toplevel_drag_attach(&mut self, toplevel: XdgToplevel, drag_state: ToplevelDragData);

    /// Access the currently active toplevel drag state (if any).
    /// This is read by the DnD grab to position the window during motion events.
    fn toplevel_drag_state(&self) -> Option<ToplevelDragData>;
}

/// Manager for the xdg-toplevel-drag-v1 global.
#[derive(Debug)]
pub struct XdgToplevelDragManager {
    global: GlobalId,
}

impl XdgToplevelDragManager {
    /// Create the `xdg_toplevel_drag_manager_v1` global.
    pub fn new<D>(display: &DisplayHandle) -> Self
    where
        D: GlobalDispatch<XdgToplevelDragManagerV1, GlobalData>,
        D: Dispatch<XdgToplevelDragManagerV1, GlobalData>,
        D: Dispatch<XdgToplevelDragV1, XdgToplevelDragUserData>,
        D: XdgToplevelDragHandler,
    {
        let global = display.create_global::<D, XdgToplevelDragManagerV1, _>(1, GlobalData);
        Self { global }
    }

    /// Returns the global ID.
    pub fn global(&self) -> GlobalId {
        self.global.clone()
    }
}

impl<D> GlobalDispatch2<XdgToplevelDragManagerV1, D> for GlobalData
where
    D: Dispatch<XdgToplevelDragManagerV1, GlobalData>,
    D: XdgToplevelDragHandler,
{
    fn bind(
        &self,
        _state: &mut D,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<XdgToplevelDragManagerV1>,
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(resource, GlobalData);
    }
}

impl<D> Dispatch2<XdgToplevelDragManagerV1, D> for GlobalData
where
    D: Dispatch<XdgToplevelDragV1, XdgToplevelDragUserData>,
    D: XdgToplevelDragHandler,
{
    fn request(
        &self,
        _state: &mut D,
        _client: &Client,
        resource: &XdgToplevelDragManagerV1,
        request: xdg_toplevel_drag_manager_v1::Request,
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            xdg_toplevel_drag_manager_v1::Request::GetXdgToplevelDrag { id, data_source } => {
                // Protocol: a data_source must not be used for more than one toplevel drag.
                // The mark lives on the source itself, so it goes away with the source; it
                // also makes set_selection refuse the source (per spec: "Attempting to use
                // the source other than for drag-and-drop such as in
                // wl_data_device.set_selection will raise an invalid_source error.")
                if let Some(source_data) = data_source.data::<DataSourceUserData>() {
                    if source_data.toplevel_drag_source.swap(true, Ordering::AcqRel) {
                        resource.post_error(
                            xdg_toplevel_drag_manager_v1::Error::InvalidSource,
                            "This data_source already has an xdg_toplevel_drag associated",
                        );
                        return;
                    }
                }

                let user_data = XdgToplevelDragUserData {
                    state: Arc::new(Mutex::new(ToplevelDragState::default())),
                    data_source,
                };
                data_init.init(id, user_data);
            }
            xdg_toplevel_drag_manager_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }
}

impl<D> Dispatch2<XdgToplevelDragV1, D> for XdgToplevelDragUserData
where
    D: XdgToplevelDragHandler,
{
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        resource: &XdgToplevelDragV1,
        request: xdg_toplevel_drag_v1::Request,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            xdg_toplevel_drag_v1::Request::Attach {
                toplevel,
                x_offset,
                y_offset,
            } => {
                let mut drag = self.state.lock().unwrap();
                if drag.toplevel.is_some() {
                    resource.post_error(
                        xdg_toplevel_drag_v1::Error::ToplevelAttached,
                        "A toplevel is already attached to this drag",
                    );
                    return;
                }
                // Extract the wl_surface from the toplevel's user data
                let wl_surface = toplevel
                    .data::<XdgShellSurfaceUserData>()
                    .map(|d| d.wl_surface.clone());
                drag.toplevel = Some(toplevel.clone());
                drag.wl_surface = wl_surface;
                drag.x_offset = x_offset;
                drag.y_offset = y_offset;
                drop(drag);

                state.toplevel_drag_attach(toplevel, self.state.clone());
            }
            xdg_toplevel_drag_v1::Request::Destroy => {
                let drag = self.state.lock().unwrap();
                if !drag.drag_ended {
                    // Be lenient: if the grab was terminated abnormally (e.g. seat
                    // destruction or grab replacement), dropped() may never have been
                    // called, leaving drag_ended false. Posting a protocol error here
                    // would prevent the client from cleaning up the resource, causing
                    // a leak. Allow the destroy and just log it.
                    tracing::debug!(
                        "xdg_toplevel_drag_v1 destroyed before drag_ended was set; \
                         grab may have ended abnormally"
                    );
                }
            }
            _ => unreachable!(),
        }
    }
}
