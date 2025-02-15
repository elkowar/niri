use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::{env, io, process};

use anyhow::Context;
use calloop::io::Async;
use directories::BaseDirs;
use futures_util::io::{AsyncReadExt, BufReader};
use futures_util::{AsyncBufReadExt, AsyncWriteExt};
use niri_ipc::{KeyboardLayouts, OutputConfigChanged, Reply, Request, Response};
use smithay::desktop::Window;
use smithay::input::keyboard::XkbContextHandler;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction};
use smithay::reexports::rustix::fs::unlink;
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;

use crate::backend::IpcOutputMap;
use crate::niri::State;
use crate::utils::version;

pub struct IpcServer {
    pub socket_path: PathBuf,
}

struct ClientCtx {
    event_loop: LoopHandle<'static, State>,
    ipc_outputs: Arc<Mutex<IpcOutputMap>>,
    ipc_focused_window: Arc<Mutex<Option<Window>>>,
}

impl IpcServer {
    pub fn start(
        event_loop: &LoopHandle<'static, State>,
        wayland_socket_name: &str,
    ) -> anyhow::Result<Self> {
        let _span = tracy_client::span!("Ipc::start");

        let socket_name = format!("niri.{wayland_socket_name}.{}.sock", process::id());
        let mut socket_path = socket_dir();
        socket_path.push(socket_name);

        let listener = UnixListener::bind(&socket_path).context("error binding socket")?;
        listener
            .set_nonblocking(true)
            .context("error setting socket to non-blocking")?;

        let source = Generic::new(listener, Interest::READ, Mode::Level);
        event_loop
            .insert_source(source, |_, socket, state| {
                match socket.accept() {
                    Ok((stream, _)) => on_new_ipc_client(state, stream),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => (),
                    Err(e) => return Err(e),
                }

                Ok(PostAction::Continue)
            })
            .unwrap();

        Ok(Self { socket_path })
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        let _ = unlink(&self.socket_path);
    }
}

fn socket_dir() -> PathBuf {
    BaseDirs::new()
        .as_ref()
        .and_then(|x| x.runtime_dir())
        .map(|x| x.to_owned())
        .unwrap_or_else(env::temp_dir)
}

fn on_new_ipc_client(state: &mut State, stream: UnixStream) {
    let _span = tracy_client::span!("on_new_ipc_client");
    trace!("new IPC client connected");

    let stream = match state.niri.event_loop.adapt_io(stream) {
        Ok(stream) => stream,
        Err(err) => {
            warn!("error making IPC stream async: {err:?}");
            return;
        }
    };

    let ctx = ClientCtx {
        event_loop: state.niri.event_loop.clone(),
        ipc_outputs: state.backend.ipc_outputs(),
        ipc_focused_window: state.niri.ipc_focused_window.clone(),
    };

    let future = async move {
        if let Err(err) = handle_client(ctx, stream).await {
            warn!("error handling IPC client: {err:?}");
        }
    };
    if let Err(err) = state.niri.scheduler.schedule(future) {
        warn!("error scheduling IPC stream future: {err:?}");
    }
}

async fn handle_client(ctx: ClientCtx, stream: Async<'_, UnixStream>) -> anyhow::Result<()> {
    let (read, mut write) = stream.split();
    let mut buf = String::new();

    // Read a single line to allow extensibility in the future to keep reading.
    BufReader::new(read)
        .read_line(&mut buf)
        .await
        .context("error reading request")?;

    let request = serde_json::from_str(&buf)
        .context("error parsing request")
        .map_err(|err| err.to_string());
    let requested_error = matches!(request, Ok(Request::ReturnError));

    let reply = match request {
        Ok(request) => process(&ctx, request).await,
        Err(err) => Err(err),
    };

    if let Err(err) = &reply {
        if !requested_error {
            warn!("error processing IPC request: {err:?}");
        }
    }

    let buf = serde_json::to_vec(&reply).context("error formatting reply")?;
    write.write_all(&buf).await.context("error writing reply")?;

    Ok(())
}

async fn process(ctx: &ClientCtx, request: Request) -> Reply {
    let response = match request {
        Request::ReturnError => return Err(String::from("example compositor error")),
        Request::Version => Response::Version(version()),
        Request::Outputs => {
            let ipc_outputs = ctx.ipc_outputs.lock().unwrap().clone();
            let outputs = ipc_outputs.values().cloned().map(|o| (o.name.clone(), o));
            Response::Outputs(outputs.collect())
        }
        Request::FocusedWindow => {
            let window = ctx.ipc_focused_window.lock().unwrap().clone();
            let window = window.map(|window| {
                let wl_surface = window.toplevel().expect("no X11 support").wl_surface();
                with_states(wl_surface, |states| {
                    let role = states
                        .data_map
                        .get::<XdgToplevelSurfaceData>()
                        .unwrap()
                        .lock()
                        .unwrap();

                    niri_ipc::Window {
                        title: role.title.clone(),
                        app_id: role.app_id.clone(),
                    }
                })
            });
            Response::FocusedWindow(window)
        }
        Request::Action(action) => {
            let (tx, rx) = async_channel::bounded(1);

            let action = niri_config::Action::from(action);
            ctx.event_loop.insert_idle(move |state| {
                state.do_action(action, false);
                let _ = tx.send_blocking(());
            });

            // Wait until the action has been processed before returning. This is important for a
            // few actions, for instance for DoScreenTransition this wait ensures that the screen
            // contents were sampled into the texture.
            let _ = rx.recv().await;
            Response::Handled
        }
        Request::Output { output, action } => {
            let ipc_outputs = ctx.ipc_outputs.lock().unwrap();
            let found = ipc_outputs
                .values()
                .any(|o| o.name.eq_ignore_ascii_case(&output));
            let response = if found {
                OutputConfigChanged::Applied
            } else {
                OutputConfigChanged::OutputWasMissing
            };
            drop(ipc_outputs);

            ctx.event_loop.insert_idle(move |state| {
                state.apply_transient_output_config(&output, action);
            });

            Response::OutputConfigChanged(response)
        }
        Request::Workspaces => {
            let (tx, rx) = async_channel::bounded(1);
            ctx.event_loop.insert_idle(move |state| {
                let workspaces = state.niri.layout.ipc_workspaces();
                let _ = tx.send_blocking(workspaces);
            });
            let result = rx.recv().await;
            let workspaces = result.map_err(|_| String::from("error getting workspace info"))?;
            Response::Workspaces(workspaces)
        }
        Request::FocusedOutput => {
            let (tx, rx) = async_channel::bounded(1);
            ctx.event_loop.insert_idle(move |state| {
                let active_output = state
                    .niri
                    .layout
                    .active_output()
                    .map(|output| output.name());

                let output = active_output.and_then(|active_output| {
                    state
                        .backend
                        .ipc_outputs()
                        .lock()
                        .unwrap()
                        .values()
                        .find(|o| o.name == active_output)
                        .cloned()
                });

                let _ = tx.send_blocking(output);
            });
            let result = rx.recv().await;
            let output = result.map_err(|_| String::from("error getting active output info"))?;
            Response::FocusedOutput(output)
        }
        Request::KeyboardLayouts => {
            let (tx, rx) = async_channel::bounded(1);
            ctx.event_loop.insert_idle(move |state| {
                let keyboard = state.niri.seat.get_keyboard().unwrap();
                let layout = keyboard.with_xkb_state(state, |context| {
                    let layouts = context.keymap().layouts();
                    KeyboardLayouts {
                        names: layouts.map(str::to_owned).collect(),
                        current_idx: context.active_layout().0 as u8,
                    }
                });
                let _ = tx.send_blocking(layout);
            });
            let result = rx.recv().await;
            let layout = result.map_err(|_| String::from("error getting layout info"))?;
            Response::KeyboardLayouts(layout)
        }
    };

    Ok(response)
}
