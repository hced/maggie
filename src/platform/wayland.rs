//! Wayland platform backend.
//!
//! Re-exports all Wayland-specific types from `smithay-client-toolkit` and
//! `wayland-client` so that `engine.rs` can use them without directly
//! depending on those crates. As the cross-platform migration progresses,
//! these re-exports will be replaced by trait-based abstractions.

// ── smithay-client-toolkit ──────────────────────────────────────────────

pub use smithay_client_toolkit::compositor::CompositorHandler;
pub use smithay_client_toolkit::compositor::CompositorState;
pub use smithay_client_toolkit::compositor::FrameCallbackData;
pub use smithay_client_toolkit::delegate_registry;
pub use smithay_client_toolkit::output::OutputHandler;
pub use smithay_client_toolkit::output::OutputState;
pub use smithay_client_toolkit::registry::ProvidesRegistryState;
pub use smithay_client_toolkit::registry::RegistryState;
pub use smithay_client_toolkit::registry_handlers;
pub use smithay_client_toolkit::seat::Capability;
pub use smithay_client_toolkit::seat::SeatHandler;
pub use smithay_client_toolkit::seat::SeatState;
pub use smithay_client_toolkit::seat::keyboard::KeyEvent;
pub use smithay_client_toolkit::seat::keyboard::KeyboardHandler;
pub use smithay_client_toolkit::seat::keyboard::Keysym;
pub use smithay_client_toolkit::seat::keyboard::Modifiers;
pub use smithay_client_toolkit::seat::keyboard::RawModifiers;
pub use smithay_client_toolkit::seat::pointer::PointerEvent;
pub use smithay_client_toolkit::seat::pointer::PointerEventKind;
pub use smithay_client_toolkit::seat::pointer::PointerHandler;
pub use smithay_client_toolkit::seat::pointer_constraints::PointerConstraintsHandler;
pub use smithay_client_toolkit::seat::pointer_constraints::PointerConstraintsState;
pub use smithay_client_toolkit::shell::WaylandSurface;
pub use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
pub use smithay_client_toolkit::shm::Shm;
pub use smithay_client_toolkit::shm::ShmHandler;
pub use smithay_client_toolkit::shm::slot::Buffer;
pub use smithay_client_toolkit::shm::slot::SlotPool;

// ── dispatch macros ────────────────────────────────────────────────────
pub use smithay_client_toolkit::dispatch2::Dispatch2;
pub use smithay_client_toolkit::delegate_dispatch2;

// ── wayland-client ──────────────────────────────────────────────────────

pub use wayland_client::Proxy;
pub use wayland_client::WEnum;
pub use wayland_client::globals::registry_queue_init;
pub use wayland_client::protocol::{
    wl_callback, wl_keyboard, wl_output, wl_pointer, wl_region, wl_seat, wl_shm, wl_surface,
};
pub use wayland_client::{
    Connection, Dispatch, DispatchError, EventQueue, QueueHandle,
};
pub use wayland_client::backend::WaylandError;

// ── wayland-protocols ───────────────────────────────────────────────────

pub use wayland_protocols::wp::pointer_constraints::zv1::client::{
    zwp_confined_pointer_v1::ZwpConfinedPointerV1, zwp_locked_pointer_v1::ZwpLockedPointerV1,
    zwp_pointer_constraints_v1::Lifetime,
};

pub use wayland_egl::WlEglSurface;

// ── wayland-protocols-wlr ──────────────────────────────────────────────

pub use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_frame_v1::Flags,
    zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};
