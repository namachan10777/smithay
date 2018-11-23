//! Utilities for manipulating the data devices
//!
//! The data device is wayland's abstraction to represent both selection (copy/paste) and
//! drag'n'drop actions. This module provides logic to handle this part of the protocol.
//! Selection and drag'n'drop are per-seat notions.
//!
//! This module provides 2 main freestanding functions:
//!
//! - `init_data_device`: this function must be called during the compositor startup to initialize
//!   the data device logic
//! - `set_data_device_focus`: this function sets the data device focus for a given seat; you'd
//!   typically call it whenever the keyboard focus changes, to follow it (for example in the focus
//!   hook of your keyboards)
//!
//! Using these two functions is enough for your clients to be able to interact with each other using
//! the data devices.
//!
//! The module also provides additionnal mechanisms allowing your compositor to see and interact with
//! the contents of the data device:
//!
//! - You can provide a callback closure to `init_data_device` to peek into the the actions of your clients
//! - the freestanding function `set_data_device_selection` allows you to set the contents of the selection
//!   for your clients
//! - the freestanding function `start_dnd` allows you to initiate a drag'n'drop event from the compositor
//!   itself and receive interactions of clients with it via an other dedicated callback.
//!
//! ## Initialization
//!
//! ```
//! # extern crate wayland_server;
//! # #[macro_use] extern crate smithay;
//! use smithay::wayland::data_device::{init_data_device, default_action_chooser};
//!
//! # fn main(){
//! # let mut event_loop = wayland_server::calloop::EventLoop::<()>::new().unwrap();
//! # let mut display = wayland_server::Display::new(event_loop.handle());
//! // init the data device:
//! init_data_device(
//!     &mut display,           // the display
//!     |dnd_event| { /* a callback to react to client DnD/selection actions */ },
//!     default_action_chooser, // a closure to choose the DnD action depending on clients
//!                             // negociation
//!     None                    // insert a logger here
//! );
//! # }
//! ```

use std::os::unix::io::RawFd;
use std::sync::{Arc, Mutex};

use wayland_server::{
    protocol::{
        wl_data_device,
        wl_data_device_manager::{self, DndAction},
        wl_data_offer, wl_data_source,
    },
    Client, Display, Global, NewResource, Resource,
};

use wayland::seat::Seat;

mod data_source;
mod dnd_grab;
mod server_dnd_grab;

pub use self::data_source::{with_source_metadata, SourceMetadata};
pub use self::server_dnd_grab::ServerDndEvent;

/// Events that are generated by interactions of the clients with the data device
pub enum DataDeviceEvent {
    /// A client has set the selection
    NewSelection(Option<Resource<wl_data_source::WlDataSource>>),
    /// A client started a drag'n'drop as response to a user pointer action
    DnDStarted(Option<Resource<wl_data_source::WlDataSource>>),
    /// A client requested to read the server-set selection
    SendSelection {
        /// the requested mime type
        mime_type: String,
        /// the fd to write into
        fd: RawFd,
    },
}

enum Selection {
    Empty,
    Client(Resource<wl_data_source::WlDataSource>),
    Compositor(SourceMetadata),
}

struct SeatData {
    known_devices: Vec<Resource<wl_data_device::WlDataDevice>>,
    selection: Selection,
    log: ::slog::Logger,
    current_focus: Option<Client>,
}

impl SeatData {
    fn set_selection(&mut self, new_selection: Selection) {
        self.selection = new_selection;
        self.send_selection();
    }

    fn set_focus(&mut self, new_focus: Option<Client>) {
        self.current_focus = new_focus;
        self.send_selection();
    }

    fn send_selection(&mut self) {
        let client = match self.current_focus.as_ref() {
            Some(c) => c,
            None => return,
        };
        // first sanitize the selection, reseting it to null if the client holding
        // it dropped it
        let cleanup = if let Selection::Client(ref data_source) = self.selection {
            !data_source.is_alive()
        } else {
            false
        };
        if cleanup {
            self.selection = Selection::Empty;
        }
        // then send it if appropriate
        match self.selection {
            Selection::Empty => {
                // send an empty selection
                for dd in &self.known_devices {
                    // skip data devices not belonging to our client
                    if dd.client().map(|c| !c.equals(client)).unwrap_or(true) {
                        continue;
                    }
                    dd.send(wl_data_device::Event::Selection { id: None });
                }
            }
            Selection::Client(ref data_source) => {
                for dd in &self.known_devices {
                    // skip data devices not belonging to our client
                    if dd.client().map(|c| !c.equals(client)).unwrap_or(true) {
                        continue;
                    }
                    let source = data_source.clone();
                    let log = self.log.clone();
                    // create a corresponding data offer
                    let offer = client
                        .create_resource::<wl_data_offer::WlDataOffer>(dd.version())
                        .unwrap()
                        .implement(
                            move |req, _offer| match req {
                                wl_data_offer::Request::Receive { fd, mime_type } => {
                                    // check if the source and associated mime type is still valid
                                    let valid = with_source_metadata(&source, |meta| {
                                        meta.mime_types.contains(&mime_type)
                                    }).unwrap_or(false)
                                        && source.is_alive();
                                    if !valid {
                                        // deny the receive
                                        debug!(log, "Denying a wl_data_offer.receive with invalid source.");
                                    } else {
                                        source.send(wl_data_source::Event::Send { mime_type, fd });
                                    }
                                    let _ = ::nix::unistd::close(fd);
                                }
                                _ => { /* seleciton data offers only care about the `receive` event */ }
                            },
                            None::<fn(_)>,
                            (),
                        );
                    // advertize the offer to the client
                    dd.send(wl_data_device::Event::DataOffer { id: offer.clone() });
                    with_source_metadata(data_source, |meta| {
                        for mime_type in meta.mime_types.iter().cloned() {
                            offer.send(wl_data_offer::Event::Offer { mime_type })
                        }
                    }).unwrap();
                    dd.send(wl_data_device::Event::Selection { id: Some(offer) });
                }
            }
            Selection::Compositor(ref meta) => {
                for dd in &self.known_devices {
                    // skip data devices not belonging to our client
                    if dd.client().map(|c| !c.equals(client)).unwrap_or(true) {
                        continue;
                    }
                    let log = self.log.clone();
                    let offer_meta = meta.clone();
                    let callback = dd.user_data::<DataDeviceData>().unwrap().callback.clone();
                    // create a corresponding data offer
                    let offer = client
                        .create_resource::<wl_data_offer::WlDataOffer>(dd.version())
                        .unwrap()
                        .implement(
                            move |req, _offer| match req {
                                wl_data_offer::Request::Receive { fd, mime_type } => {
                                    // check if the associated mime type is valid
                                    if !offer_meta.mime_types.contains(&mime_type) {
                                        // deny the receive
                                        debug!(log, "Denying a wl_data_offer.receive with invalid source.");
                                        let _ = ::nix::unistd::close(fd);
                                    } else {
                                        (&mut *callback.lock().unwrap())(DataDeviceEvent::SendSelection {
                                            mime_type,
                                            fd,
                                        });
                                    }
                                }
                                _ => { /* seleciton data offers only care about the `receive` event */ }
                            },
                            None::<fn(_)>,
                            (),
                        );
                    // advertize the offer to the client
                    dd.send(wl_data_device::Event::DataOffer { id: offer.clone() });
                    for mime_type in meta.mime_types.iter().cloned() {
                        offer.send(wl_data_offer::Event::Offer { mime_type })
                    }
                    dd.send(wl_data_device::Event::Selection { id: Some(offer) });
                }
            }
        }
    }
}

impl SeatData {
    fn new(log: ::slog::Logger) -> SeatData {
        SeatData {
            known_devices: Vec::new(),
            selection: Selection::Empty,
            log,
            current_focus: None,
        }
    }
}

/// Initialize the data device global
///
/// You can provide a callback to peek into the actions of your clients over the data devices
/// (allowing you to retrieve the current selection buffer, or intercept DnD data). See the
/// `DataDeviceEvent` type for details about what notifications you can receive. Note that this
/// closure will not receive notifications about dnd actions the compositor initiated, see
/// `start_dnd` for details about that.
///
/// You also need to provide a `(DndAction, DndAction) -> DndAction` closure that will arbitrate
/// the choice of action resulting from a drag'n'drop session. Its first argument is the set of
/// available actions (which is the intersection of the actions supported by the source and targets)
/// and the second argument is the preferred action reported by the target. If no action should be
/// chosen (and thus the drag'n'drop should abort on drop), return `DndAction::empty()`.
pub fn init_data_device<F, C, L>(
    display: &mut Display,
    callback: C,
    action_choice: F,
    logger: L,
) -> Global<wl_data_device_manager::WlDataDeviceManager>
where
    F: FnMut(DndAction, DndAction) -> DndAction + Send + 'static,
    C: FnMut(DataDeviceEvent) + Send + 'static,
    L: Into<Option<::slog::Logger>>,
{
    let log = ::slog_or_stdlog(logger).new(o!("smithay_module" => "data_device_mgr"));
    let action_choice = Arc::new(Mutex::new(action_choice));
    let callback = Arc::new(Mutex::new(callback));
    let global = display.create_global(3, move |new_ddm, _version| {
        implement_ddm(new_ddm, callback.clone(), action_choice.clone(), log.clone());
    });

    global
}

/// Set the data device focus to a certain client for a given seat
pub fn set_data_device_focus(seat: &Seat, client: Option<Client>) {
    // ensure the seat user_data is ready
    // TODO: find a better way to retrieve a logger without requiring the user
    // to provide one ?
    // This should be a rare path anyway, it is unlikely that a client gets focus
    // before initializing its data device, which would already init the user_data.
    seat.user_data().insert_if_missing(|| {
        Mutex::new(SeatData::new(
            seat.arc.log.new(o!("smithay_module" => "data_device_mgr")),
        ))
    });
    let seat_data = seat.user_data().get::<Mutex<SeatData>>().unwrap();
    seat_data.lock().unwrap().set_focus(client);
}

/// Set a compositor-provided selection for this seat
///
/// You need to provide the available mime types for this selection.
///
/// Whenever a client requests to read the selection, your callback will
/// receive a `DataDeviceEvent::SendSelection` event.
pub fn set_data_device_selection(seat: &Seat, mime_types: Vec<String>) {
    // TODO: same question as in set_data_device_focus
    seat.user_data().insert_if_missing(|| {
        Mutex::new(SeatData::new(
            seat.arc.log.new(o!("smithay_module" => "data_device_mgr")),
        ))
    });
    let seat_data = seat.user_data().get::<Mutex<SeatData>>().unwrap();
    seat_data
        .lock()
        .unwrap()
        .set_selection(Selection::Compositor(SourceMetadata {
            mime_types,
            dnd_action: DndAction::empty(),
        }));
}

/// Start a drag'n'drop from a ressource controlled by the compositor
///
/// You'll receive events generated by the interaction of clients with your
/// drag'n'drop in the provided callback. See `SeverDndEvent` for details about
/// which events can be generated and what response is expected from you to them.
pub fn start_dnd<C>(seat: &Seat, serial: u32, metadata: SourceMetadata, callback: C)
where
    C: FnMut(ServerDndEvent) + Send + 'static,
{
    // TODO: same question as in set_data_device_focus
    seat.user_data().insert_if_missing(|| {
        Mutex::new(SeatData::new(
            seat.arc.log.new(o!("smithay_module" => "data_device_mgr")),
        ))
    });
    if let Some(pointer) = seat.get_pointer() {
        pointer.set_grab(
            server_dnd_grab::ServerDnDGrab::new(metadata, seat.clone(), Arc::new(Mutex::new(callback))),
            serial,
        );
        return;
    }
}

fn implement_ddm<F, C>(
    new_ddm: NewResource<wl_data_device_manager::WlDataDeviceManager>,
    callback: Arc<Mutex<C>>,
    action_choice: Arc<Mutex<F>>,
    log: ::slog::Logger,
) -> Resource<wl_data_device_manager::WlDataDeviceManager>
where
    F: FnMut(DndAction, DndAction) -> DndAction + Send + 'static,
    C: FnMut(DataDeviceEvent) + Send + 'static,
{
    use self::wl_data_device_manager::Request;
    new_ddm.implement(
        move |req, _ddm| match req {
            Request::CreateDataSource { id } => {
                self::data_source::implement_data_source(id);
            }
            Request::GetDataDevice { id, seat } => match Seat::from_resource(&seat) {
                Some(seat) => {
                    // ensure the seat user_data is ready
                    seat.user_data()
                        .insert_if_missing(|| Mutex::new(SeatData::new(log.clone())));
                    let seat_data = seat.user_data().get::<Mutex<SeatData>>().unwrap();
                    let data_device = implement_data_device(
                        id,
                        seat.clone(),
                        callback.clone(),
                        action_choice.clone(),
                        log.clone(),
                    );
                    seat_data.lock().unwrap().known_devices.push(data_device);
                }
                None => {
                    error!(log, "Unmanaged seat given to a data device.");
                }
            },
        },
        None::<fn(_)>,
        (),
    )
}

struct DataDeviceData {
    callback: Arc<Mutex<FnMut(DataDeviceEvent) + Send + 'static>>,
    action_choice: Arc<Mutex<FnMut(DndAction, DndAction) -> DndAction + Send + 'static>>,
}

fn implement_data_device<F, C>(
    new_dd: NewResource<wl_data_device::WlDataDevice>,
    seat: Seat,
    callback: Arc<Mutex<C>>,
    action_choice: Arc<Mutex<F>>,
    log: ::slog::Logger,
) -> Resource<wl_data_device::WlDataDevice>
where
    F: FnMut(DndAction, DndAction) -> DndAction + Send + 'static,
    C: FnMut(DataDeviceEvent) + Send + 'static,
{
    use self::wl_data_device::Request;
    let dd_data = DataDeviceData {
        callback: callback.clone(),
        action_choice,
    };
    new_dd.implement(
        move |req, dd| match req {
            Request::StartDrag {
                source,
                origin,
                icon: _,
                serial,
            } => {
                /* TODO: handle the icon */
                if let Some(pointer) = seat.get_pointer() {
                    if pointer.has_grab(serial) {
                        // The StartDrag is in response to a pointer implicit grab, all is good
                        (&mut *callback.lock().unwrap())(DataDeviceEvent::DnDStarted(source.clone()));
                        pointer.set_grab(dnd_grab::DnDGrab::new(source, origin, seat.clone()), serial);
                        return;
                    }
                }
                debug!(log, "denying drag from client without implicit grab");
            }
            Request::SetSelection { source, serial: _ } => {
                if let Some(keyboard) = seat.get_keyboard() {
                    if dd
                        .client()
                        .as_ref()
                        .map(|c| keyboard.has_focus(c))
                        .unwrap_or(false)
                    {
                        let seat_data = seat.user_data().get::<Mutex<SeatData>>().unwrap();
                        (&mut *callback.lock().unwrap())(DataDeviceEvent::NewSelection(source.clone()));
                        // The client has kbd focus, it can set the selection
                        seat_data
                            .lock()
                            .unwrap()
                            .set_selection(source.map(Selection::Client).unwrap_or(Selection::Empty));
                        return;
                    }
                }
                debug!(log, "denying setting selection by a non-focused client");
            }
            Request::Release => {
                // Clean up the known devices
                seat.user_data()
                    .get::<Mutex<SeatData>>()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .known_devices
                    .retain(|ndd| ndd.is_alive() && (!ndd.equals(&dd)))
            }
        },
        None::<fn(_)>,
        dd_data,
    )
}

/// A simple action chooser for DnD negociation
///
/// If the preferred action is available, it'll pick it. Otherwise, it'll pick the first
/// available in the following order: Ask, Copy, Move.
pub fn default_action_chooser(available: DndAction, preferred: DndAction) -> DndAction {
    // if the preferred action is valid (a single action) and in the available actions, use it
    // otherwise, follow a fallback stategy
    if [DndAction::Move, DndAction::Copy, DndAction::Ask].contains(&preferred)
        && available.contains(preferred)
    {
        preferred
    } else if available.contains(DndAction::Ask) {
        DndAction::Ask
    } else if available.contains(DndAction::Copy) {
        DndAction::Copy
    } else if available.contains(DndAction::Move) {
        DndAction::Move
    } else {
        DndAction::empty()
    }
}