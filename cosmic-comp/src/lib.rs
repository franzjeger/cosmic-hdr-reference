#![allow(
    clippy::too_many_arguments,
    clippy::type_complexity,
    clippy::len_without_is_empty,
    clippy::collapsible_match
)]
// SPDX-License-Identifier: GPL-3.0-only

use calloop::timer::{TimeoutAction, Timer};
use nix::sys::signal::{SigSet, Signal};
use smithay::{
    reexports::{
        calloop::{EventLoop, Interest, Mode, PostAction, generic::Generic},
        wayland_server::{Display, DisplayHandle},
    },
    wayland::socket::ListeningSocketSource,
};

use anyhow::{Context, Result};
use state::{BackendData, LastRefresh, State};
use std::{
    env,
    ffi::OsString,
    os::unix::process::CommandExt,
    process,
    sync::Arc,
    time::{Duration, Instant},
};
use tracing::{error, info, warn};
use wayland::protocols::{
    keyboard_layout::KeyboardLayoutState, overlap_notify::OverlapNotifyState,
};

use crate::wayland::handlers::compositor::client_compositor_state;

use clap_lex::RawArgs;

use std::error::Error;

pub mod backend;
pub mod config;
pub mod dbus;
#[cfg(feature = "debug")]
pub mod debug;
pub mod hooks;
pub mod input;
pub mod libei;
mod logger;
pub mod session;
pub mod shell;
pub mod state;
#[cfg(feature = "systemd")]
pub mod systemd;
pub mod theme;
pub mod utils;
pub mod wayland;
pub mod xwayland;

#[cfg(feature = "profile-with-tracy")]
#[global_allocator]
static GLOBAL: profiling::tracy_client::ProfiledAllocator<std::alloc::System> =
    profiling::tracy_client::ProfiledAllocator::new(std::alloc::System, 10);

// called by the Xwayland source, either after starting or failing
impl State {
    fn notify_ready(&mut self) {
        // TODO: Don't notify again, but potentially import updated env-variables
        // into systemd and the session?
        self.ready.call_once(|| {
            // potentially tell systemd we are setup now
            if let state::BackendData::Kms(_) = &self.backend {
                #[cfg(feature = "systemd")]
                systemd::ready(&self.common);
                if let Err(err) = dbus::ready(&self.common) {
                    error!(?err, "Failed to update the D-Bus activation environment");
                }
            }

            // potentially tell the session we are setup now
            if let Err(err) =
                session::run_socket(self.common.event_loop_handle.clone(), &self.common)
            {
                warn!(?err, "Failed to setup cosmic-session communication");
            }

            self.common.kiosk_child = if let Some(mut command) = self.kiosk_command.take() {
                // Run command in kiosk mode
                command.envs(
                    session::get_env(&self.common).expect("WAYLAND_DISPLAY should be valid UTF-8"),
                );
                unsafe {
                    command.pre_exec(|| {
                        utils::rlimit::restore_nofile_limit();
                        Ok(())
                    })
                };

                info!("Running {:?}", command.get_program());
                command
                    .spawn()
                    .map_err(|err| {
                        // TODO: replace with `inspect_err` once stable
                        error!(?err, "Error running kiosk child.");
                        err
                    })
                    .ok()
            } else {
                None
            };
        });
    }
}

pub fn run(hooks: crate::hooks::Hooks) -> Result<(), Box<dyn Error>> {
    // Block termination signals before logger, state, or renderer setup can
    // create threads. A dedicated waiter below can stop surface threads even if
    // the main event loop is wedged in a client or configuration callback.
    let hdr_policy = utils::env::hdr_policy();
    // Clean SIGTERM shutdown through the event loop restores persistent KMS
    // color state before the greeter takes over - correct for every session,
    // not just HDR test runs.
    let hdr_shutdown_signals = {
        let mut signals = SigSet::empty();
        signals.add(Signal::SIGTERM);
        signals.add(Signal::SIGINT);
        signals.thread_block()?;
        // cosmic-session SIGKILLs its direct child at logout, skipping cleanup.
        // The launcher interposes a wrapper process as that child; parent death
        // then delivers our ordinary SIGTERM shutdown instead.
        if let Err(err) = nix::sys::prctl::set_pdeathsig(Signal::SIGTERM) {
            warn!(?err, "failed to arm parent-death SIGTERM cleanup");
        }
        Some(signals)
    };

    let raw_args = RawArgs::from_args();
    let mut cursor = raw_args.cursor();
    raw_args.next_os(&mut cursor);
    let git_hash = option_env!("GIT_HASH").unwrap_or("unknown");

    let mut kiosk_command = None;
    let mut with_xwayland = true;
    // Parse the arguments
    while let Some(arg) = raw_args.next_os(&mut cursor) {
        match arg.to_str() {
            Some("--help") | Some("-h") => {
                print_help(env!("CARGO_PKG_VERSION"), git_hash);
                return Ok(());
            }
            Some("--no-xwayland") => {
                tracing::info!("Running without Xwayland");
                with_xwayland = false;
            }
            Some("--version") | Some("-V") => {
                println!(
                    "cosmic-comp {} (git commit {})",
                    env!("CARGO_PKG_VERSION"),
                    git_hash
                );
                return Ok(());
            }
            _ => {
                let mut cmd = process::Command::new(arg);
                cmd.args(raw_args.remaining(&mut cursor));
                kiosk_command = Some(cmd);
            }
        }
    }

    // setup logger
    logger::init_logger()?;
    info!("Cosmic starting up!");

    profiling::register_thread!("Main Thread");
    #[cfg(feature = "profile-with-tracy")]
    tracy_client::Client::start();

    utils::rlimit::increase_nofile_limit();
    // This needs to be done before any potential program launches
    // (e.g. Xwayland) as it handles passed file descriptors.
    if let Err(err) = session::setup_socket() {
        warn!("Session error: {:?}", err);
    };

    // init hook globals
    hooks::HOOKS.set(hooks)
        .expect("Hooks global has already been initialized. Running multiple instances of COSMIC in one process is not supported.");

    // init event loop
    let mut event_loop = EventLoop::try_new().with_context(|| "Failed to initialize event loop")?;
    // init wayland
    let (display, socket) = init_wayland_display(&mut event_loop)?;
    // init state
    let mut state = state::State::new(
        &display,
        socket,
        event_loop.handle(),
        event_loop.get_signal(),
        with_xwayland,
        kiosk_command,
    );
    if let Some(hdr_shutdown_signals) = hdr_shutdown_signals {
        let (signal_sender, signal_receiver) = calloop::channel::channel();
        event_loop
            .handle()
            .insert_source(signal_receiver, |event, &mut (), state| {
                let calloop::channel::Event::Msg(signal) = event else {
                    return;
                };
                warn!(?signal, "ending experimental HDR session cleanly");
                state.common.should_stop = true;
                state.common.event_loop_signal.stop();
                state.common.event_loop_signal.wakeup();
            })
            .map_err(|err| err.error)
            .context("Failed to install experimental HDR shutdown signals")?;

        std::thread::Builder::new()
            .name("cosmic-hdr-shutdown".into())
            .spawn(move || match hdr_shutdown_signals.wait() {
                Ok(signal) => {
                    // KMS cleanup starts immediately on the owning surface threads. The
                    // main loop receives the same request for ordinary state teardown.
                    backend::kms::emergency_shutdown_hdr_surfaces();
                    let _ = signal_sender.send(signal);
                }
                Err(err) => error!(?err, "strict HDR signal waiter failed"),
            })
            .context("Failed to start experimental HDR shutdown waiter")?;
    }
    // Set up the libei sender side before the backend spawns Xwayland.
    let ei_sender = libei::setup_ei(&event_loop.handle());
    state.common.dbus_state.set_ei_sender(ei_sender);

    // init backend
    if let Err(err) = backend::init_backend_auto(&display, &mut event_loop, &mut state) {
        if hdr_policy.require_active {
            // cosmic-session restarts compositors that exit non-zero. A strict HDR
            // preflight failure must instead announce the Wayland environment and end
            // successfully. The announcement unblocks cosmic-session's startup handshake;
            // the successful exit then asks it to end the session instead of restarting.
            error!(
                ?err,
                "strict HDR preflight failed; returning safely to the display manager"
            );
            if let Err(session_err) =
                session::run_socket(state.common.event_loop_handle.clone(), &state.common)
            {
                warn!(
                    ?session_err,
                    "failed to complete cosmic-session handshake during safe shutdown"
                );
            }
            if std::env::var_os("COSMIC_SESSION_SOCK").is_some() {
                // cosmic-session 1.7.0 has no acknowledgement for this handshake and can
                // panic if the compositor exits while it is still launching components.
                // Give it a bounded grace period to reach its request loop; then our status
                // 0 is handled as SessionRequest::Exit. This only applies to strict HDR
                // failure and never delays a running desktop.
                let grace_ms = hdr_policy.safe_exit_grace.as_millis();
                warn!(grace_ms, "waiting for cosmic-session safe-exit readiness");
                let deadline = Instant::now() + hdr_policy.safe_exit_grace;
                while !state.common.should_stop {
                    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                        break;
                    };
                    // Dispatching keeps the signal source live during the grace period and
                    // also lets cosmic-session finish its socket handshake.
                    event_loop.dispatch(Some(remaining), &mut state)?;
                }
            }
            return Ok(());
        }
        return Err(err.into());
    }

    if let Err(err) = theme::watch_theme(event_loop.handle()) {
        warn!(?err, "Failed to watch theme");
    }

    // run the event loop
    event_loop.run(None, &mut state, |state| {
        // shall we shut down?
        if state.common.should_stop {
            info!("Shutting down");
            state.common.event_loop_signal.stop();
            state.common.event_loop_signal.wakeup();
            return;
        }

        // trigger routines
        let clients = state.common.shell.write().update_animations();
        {
            let dh = state.common.display_handle.clone();
            for client in clients.values() {
                client_compositor_state(client).blocker_cleared(state, &dh);
            }
        }

        refresh(state);

        {
            let shell = state.common.shell.read();
            if shell.animations_going() {
                for output in shell.outputs().cloned().collect::<Vec<_>>().into_iter() {
                    state.backend.schedule_render(&output);
                }
            }
        }

        // send out events
        let _ = state.common.display_handle.flush_clients();

        // check if kiosk child is running
        if let Some(child) = state.common.kiosk_child.as_mut() {
            match child.try_wait() {
                // Kiosk child exited with status
                Ok(Some(exit_status)) => {
                    info!("Command exited with status {:?}", exit_status);
                    // Stop cleanly so surface threads are joined before exit() (signal -> 1).
                    state.common.kiosk_exit_code = Some(exit_status.code().unwrap_or(1));
                    state.common.should_stop = true;
                }
                // Command still running
                Ok(None) => {}
                // Kiosk child disappeared, exiting with error
                Err(err) => {
                    warn!(?err, "Failed to wait for command");
                    state.common.kiosk_exit_code = Some(1);
                    state.common.should_stop = true;
                }
            }
        }
    })?;

    // kill kiosk child if loop exited
    if let Some(mut child) = state.common.kiosk_child.take() {
        let _ = child.kill();
    }

    let kiosk_exit_code = state.common.kiosk_exit_code;

    // Join surface threads before exit() so no thread is mid-eglCreateSync when
    // Mesa's atexit handlers run and corrupt the heap (issue #2375). Safe here
    // because the event loop has stopped; an unconditional join in Surface::Drop
    // would instead deadlock against apply_config_for_outputs.
    if let BackendData::Kms(kms) = &mut state.backend {
        // Connector color properties are persistent KMS state regardless of how HDR
        // was enabled. Stop every output while the device is active so surface drop can
        // restore Colorspace=Default and HDR_OUTPUT_METADATA=0.
        let mut surface_threads = Vec::new();
        for device in kms.drm_devices.values_mut() {
            for (_, surface) in device.inner.surfaces.drain() {
                let name = surface.output.name();
                if let Some(thread) = surface.begin_shutdown() {
                    surface_threads.push((name, thread));
                }
            }
        }

        // A wedged driver must not turn compositor shutdown into an unbounded hang.
        // All surfaces share this deadline; unfinished threads are detached after the
        // device is paused, which makes subsequent DRM cleanup observe DeviceInactive.
        let timeout_ms = hdr_policy.teardown_timeout.as_millis();
        let deadline = Instant::now() + hdr_policy.teardown_timeout;
        while !surface_threads.is_empty() && Instant::now() < deadline {
            let mut index = 0;
            while index < surface_threads.len() {
                if surface_threads[index].1.is_finished() {
                    let (name, thread) = surface_threads.swap_remove(index);
                    if let Err(err) = thread.join() {
                        warn!(output = %name, ?err, "surface thread panicked during shutdown");
                    } else {
                        info!(output = %name, "surface thread terminated after KMS cleanup");
                    }
                } else {
                    index += 1;
                }
            }
            if !surface_threads.is_empty() {
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        for device in kms.drm_devices.values_mut() {
            device.drm.pause();
        }
        for (name, _thread) in surface_threads {
            warn!(output = %name, timeout_ms, "KMS cleanup timed out; detached surface thread after pausing device");
        }
    }

    // drop eventloop & state before logger
    std::mem::drop(event_loop);
    std::mem::drop(state);

    if let Some(code) = kiosk_exit_code {
        process::exit(code);
    }

    Ok(())
}

fn print_help(version: &str, git_rev: &str) {
    println!(
        r#"cosmic-comp {version} (git commit {git_rev})
System76 <info@system76.com>

Designed for the COSMIC™ desktop environment, cosmic-comp is a Wayland Compositor.

Project home page: https://github.com/pop-os/cosmic-comp

Options:
  -h, --help          Show this message
  --no-xwayland       Run without Xwayland
  -v, --version       Show the version of cosmic-comp"#
    );
}

fn init_wayland_display(
    event_loop: &mut EventLoop<state::State>,
) -> Result<(DisplayHandle, OsString)> {
    let display = Display::new().unwrap();
    let handle = display.handle();

    let source = ListeningSocketSource::new_auto().unwrap();
    let socket_name = source.socket_name().to_os_string();
    info!("Listening on {:?}", socket_name);

    event_loop
        .handle()
        .insert_source(source, |client_stream, _, state| {
            let client_state = state.new_client_state();
            if let Err(err) = state
                .common
                .display_handle
                .insert_client(client_stream, Arc::new(client_state))
            {
                warn!(?err, "Error adding wayland client")
            };
        })
        .with_context(|| "Failed to init the wayland socket source.")?;
    event_loop
        .handle()
        .insert_source(
            Generic::new(display, Interest::READ, Mode::Level),
            move |_, display, state| {
                // SAFETY: We don't drop the display
                match unsafe { display.get_mut().dispatch_clients(state) } {
                    Ok(_) => Ok(PostAction::Continue),
                    Err(err) => {
                        error!(?err, "I/O error on the Wayland display");
                        state.common.should_stop = true;
                        Err(err)
                    }
                }
            },
        )
        .with_context(|| "Failed to init the wayland event source.")?;

    Ok((handle, socket_name))
}

fn refresh(state: &mut State) {
    if matches!(state.last_refresh, LastRefresh::Scheduled(_)) {
        return;
    }

    if matches!(state.last_refresh, LastRefresh::At(instant) if Instant::now().duration_since(instant) < Duration::from_millis(150))
    {
        if let Ok(token) = state.common.event_loop_handle.insert_source(
            Timer::from_duration(Duration::from_millis(150)),
            |_, _, state| {
                state.last_refresh = LastRefresh::None;
                TimeoutAction::Drop
            },
        ) {
            state.last_refresh = LastRefresh::Scheduled(token);
            return;
        } else {
            warn!("Failed to schedule refresh");
        }
    }

    state.common.refresh();
    state::Common::refresh_focus(state);
    OverlapNotifyState::refresh(state);
    state.common.update_x11_stacking_order();
    KeyboardLayoutState::refresh(state);
    state.last_refresh = LastRefresh::At(Instant::now());
}
