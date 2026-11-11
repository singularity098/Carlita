//! Showing plugin editors inside Carlita's own window on X11.
//!
//! Plugin editors are separate X11 windows: Carla shows them as top-level
//! windows, and with plugin bridges they even live in another process.
//! Carlita finds the editor window by the unique title it gave it and
//! *reparents* it into a child window of the main window that covers the
//! main area. From then on the editor is an ordinary part of Carlita's
//! window: it draws itself and gets real pointer and touch input, so it
//! works with every toolkit. Editors are shown at their own size (editors
//! that can resize are given the whole area).
//!
//! An X11 child window always sits above what Slint draws, so the editor is
//! hidden while a sidebar, the browser or a dialog covers the main area
//! ([`HostCommand::SetArea`] with `visible: false`).

mod host;

pub use host::{Area, EditorHost, HostCommand, HostEvent};
