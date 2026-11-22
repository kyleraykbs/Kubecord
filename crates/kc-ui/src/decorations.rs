//! The KDE transparency workaround: telling the compositor, through KDE's
//! server-decoration protocol, that it decorates the main window.
//!
//! Some compositors (niri) draw their border colour as a solid backdrop
//! behind windows that decorate themselves, which a see-through window then
//! shows all over. GTK never says how its windows are decorated, so niri
//! takes the backdrop route; saying "server-side" turns it into a plain
//! border. On compositors that really draw server-side decorations (KWin,
//! sway) the window would get a second title bar, hence a setting.
//!
//! The requests go over GTK's own Wayland connection, on a queue of our own.

use std::cell::RefCell;

use gdk4_wayland::prelude::*;
use gtk::prelude::*;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols_plasma::server_decoration::client::org_kde_kwin_server_decoration::{
    Mode, OrgKdeKwinServerDecoration,
};
use wayland_protocols_plasma::server_decoration::client::org_kde_kwin_server_decoration_manager::OrgKdeKwinServerDecorationManager;

/// The window's decoration object, and the queue its events land on.
struct Decoration {
    decoration: OrgKdeKwinServerDecoration,
    queue: EventQueue<Events>,
    events: Events,
}

/// Events are not needed; they are taken off the queue and dropped.
struct Events;

thread_local! {
    static DECORATION: RefCell<Option<Decoration>> = const { RefCell::new(None) };
    /// What was asked for last (applied once the window is shown).
    static WANTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Turns the workaround on or off for `window` (now, or once it is shown).
pub(crate) fn set_server_side(window: &impl IsA<gtk::Window>, on: bool) {
    WANTED.set(on);
    let window = window.as_ref();
    if window.is_mapped() {
        apply(window);
    }
}

/// Applies the wanted mode to `window` each time it is shown: once its
/// surface is a window (asked earlier, the compositor may not take it).
pub(crate) fn watch(window: &impl IsA<gtk::Window>) {
    let window = window.as_ref();
    // A new surface comes with each realize, and needs asking again.
    window.connect_realize(|_| DECORATION.with_borrow_mut(|slot| *slot = None));
    window.connect_map(apply);
}

fn apply(window: &gtk::Window) {
    let on = WANTED.get();
    let done = DECORATION.with_borrow_mut(|slot| {
        if slot.is_none() {
            if !on {
                // Never asked: GTK's own (unspoken) choice stands.
                return true;
            }
            *slot = create(window);
        }
        let Some(decoration) = slot else {
            return false;
        };
        decoration
            .decoration
            .request_mode(if on { Mode::Server } else { Mode::Client }.into());
        let _ = decoration.queue.dispatch_pending(&mut decoration.events);
        let _ = decoration.queue.flush();
        true
    });
    if !done {
        tracing::info!("the compositor does not offer KDE's server decorations; transparency workaround unavailable");
    }
}

/// The decoration object for `window`'s surface, if the compositor offers
/// the protocol.
fn create(window: &gtk::Window) -> Option<Decoration> {
    let surface = window.surface()?.downcast::<gdk4_wayland::WaylandSurface>().ok()?;
    let display = surface.display().downcast::<gdk4_wayland::WaylandDisplay>().ok()?;
    let wl_surface = surface.wl_surface()?;
    let wl_display = display.wl_display()?;
    let backend = wayland_client::Proxy::backend(&wl_display).upgrade()?;
    let connection = Connection::from_backend(backend);
    let (globals, queue) = registry_queue_init::<Events>(&connection).ok()?;
    let handle = queue.handle();
    let manager: OrgKdeKwinServerDecorationManager = globals.bind(&handle, 1..=1, ()).ok()?;
    let decoration = manager.create(&wl_surface, &handle, ());
    Some(Decoration {
        decoration,
        queue,
        events: Events,
    })
}

impl Dispatch<WlRegistry, GlobalListContents> for Events {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as wayland_client::Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<OrgKdeKwinServerDecorationManager, ()> for Events {
    fn event(
        _: &mut Self,
        _: &OrgKdeKwinServerDecorationManager,
        _: <OrgKdeKwinServerDecorationManager as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<OrgKdeKwinServerDecoration, ()> for Events {
    fn event(
        _: &mut Self,
        _: &OrgKdeKwinServerDecoration,
        _: <OrgKdeKwinServerDecoration as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
