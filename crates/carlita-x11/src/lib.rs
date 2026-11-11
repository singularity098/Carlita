//! Hosting plugin editors inside Carlita's own UI on X11.
//!
//! Plugin editors are separate X11 windows (Carla shows them as top-level
//! windows; with plugin bridges they even live in another process). To make
//! them fill the main area, be scaled up, and still let the sidebars slide
//! over them, Carlita never shows those windows directly. Instead it:
//!
//! 1. finds the editor window by the unique title Carlita gave it,
//! 2. moves it off-screen and redirects it with XComposite so it keeps
//!    rendering into an off-screen pixmap,
//! 3. copies that pixmap (only when XDamage reports changes) and sends the
//!    frame to the UI, which draws it scaled,
//! 4. maps touches on the scaled image back to the editor's coordinates and
//!    delivers them as synthetic pointer events to the window under them.
//!
//! This is the riskiest part of the design (see docs/editor-scaling.md):
//! some toolkits ignore synthetic events. [`InputMode::XTest`] is a fallback
//! that briefly places the editor under the pointer and uses real input.

mod host;

pub use host::{EditorFrame, EditorHost, HostCommand, HostEvent, InputMode, PointerKind};
