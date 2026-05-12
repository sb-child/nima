use mutter_x11_interop::MutterX11Interop;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::wayland::{Dispatch2, GlobalDispatch2};

use super::raw::mutter_x11_interop::v1::server::mutter_x11_interop;
use crate::niri::State;

const VERSION: u32 = 1;

pub struct MutterX11InteropManagerState {}

pub struct MutterX11InteropManagerGlobalData {
    filter: Box<dyn for<'c> Fn(&'c Client) -> bool + Send + Sync>,
}

pub trait MutterX11InteropHandler {}

impl MutterX11InteropManagerState {
    pub fn new<D, F>(display: &DisplayHandle, filter: F) -> Self
    where
        D: GlobalDispatch<MutterX11Interop, MutterX11InteropManagerGlobalData>,
        D: Dispatch<MutterX11Interop, ()>,
        D: MutterX11InteropHandler,
        D: 'static,
        F: for<'c> Fn(&'c Client) -> bool + Send + Sync + 'static,
    {
        let global_data = MutterX11InteropManagerGlobalData {
            filter: Box::new(filter),
        };
        display.create_global::<D, MutterX11Interop, _>(VERSION, global_data);

        Self {}
    }
}

impl GlobalDispatch2<MutterX11Interop, State> for MutterX11InteropManagerGlobalData {
    fn bind(
        &self,
        _state: &mut State,
        _handle: &DisplayHandle,
        _client: &Client,
        manager: New<MutterX11Interop>,
        data_init: &mut DataInit<'_, State>,
    ) {
        data_init.init(manager, ());
    }

    fn can_view(&self, client: &wayland_server::Client) -> bool {
        (self.filter)(client)
    }
}

impl Dispatch2<MutterX11Interop, State> for () {
    fn request(
        &self,
        _state: &mut State,
        _client: &Client,
        _resource: &MutterX11Interop,
        request: <MutterX11Interop as Resource>::Request,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
        match request {
            mutter_x11_interop::Request::Destroy => (),
            mutter_x11_interop::Request::SetX11Parent { .. } => (),
        }
    }
}
