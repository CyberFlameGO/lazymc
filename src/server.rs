use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::FutureExt;
use minecraft_protocol::data::server_status::ServerStatus;
use tokio::process::Command;

use crate::config::Config;

/// Shared server state.
#[derive(Default, Debug)]
pub struct ServerState {
    /// Whether the server is online.
    online: AtomicBool,

    /// Whether the server is starting.
    // TODO: use enum for starting/started/stopping states
    starting: AtomicBool,

    /// Whether the server is stopping.
    stopping: AtomicBool,

    /// Server PID.
    pid: Mutex<Option<u32>>,

    /// Last known server status.
    ///
    /// Once set, this will remain set, and isn't cleared when the server goes offline.
    // TODO: make this private?
    pub status: Mutex<Option<ServerStatus>>,

    /// Last active time.
    ///
    /// The last known time when the server was active with online players.
    last_active: Mutex<Option<Instant>>,

    /// Keep server online until.
    keep_online_until: Mutex<Option<Instant>>,
}

impl ServerState {
    /// Whether the server is online.
    pub fn online(&self) -> bool {
        self.online.load(Ordering::Relaxed)
    }

    /// Set whether the server is online.
    pub fn set_online(&self, online: bool) {
        self.online.store(online, Ordering::Relaxed)
    }

    /// Whether the server is starting.
    pub fn starting(&self) -> bool {
        self.starting.load(Ordering::Relaxed)
    }

    /// Set whether the server is starting.
    pub fn set_starting(&self, starting: bool) {
        self.starting.store(starting, Ordering::Relaxed)
    }

    /// Kill any running server.
    #[allow(unused_variables)]
    pub async fn kill_server(&self, config: &Config) -> bool {
        // Ensure we have a running process
        let has_process = self.pid.lock().unwrap().is_some();
        if !has_process {
            return false;
        }

        // Try to kill through RCON
        #[cfg(feature = "rcon")]
        if stop_server_rcon(config, &self).await {
            // TODO: set stopping state elsewhere
            self.stopping.store(true, Ordering::Relaxed);

            return true;
        }

        // Try to kill through signal
        #[cfg(unix)]
        if stop_server_signal(&self) {
            // TODO: set stopping state elsewhere
            self.stopping.store(true, Ordering::Relaxed);

            return true;
        }

        false
    }

    /// Set server PID.
    pub fn set_pid(&self, pid: Option<u32>) {
        *self.pid.lock().unwrap() = pid;
    }

    /// Clone the last known server status.
    pub fn clone_status(&self) -> Option<ServerStatus> {
        self.status.lock().unwrap().clone()
    }

    /// Update the server status.
    pub fn set_status(&self, status: ServerStatus) {
        self.status.lock().unwrap().replace(status);
    }

    /// Update the last active time.
    pub fn update_last_active_time(&self) {
        self.last_active.lock().unwrap().replace(Instant::now());
    }

    /// Update the last active time.
    pub fn set_keep_online_until(&self, duration: Option<u32>) {
        *self.keep_online_until.lock().unwrap() = duration
            .filter(|d| *d > 0)
            .map(|d| Instant::now() + Duration::from_secs(d as u64));
    }

    /// Update the server status, online state and last active time.
    // TODO: clean this up
    pub fn update_status(&self, config: &Config, status: Option<ServerStatus>) {
        let stopping = self.stopping.load(Ordering::Relaxed);
        let was_online = self.online();
        let online = status.is_some() && !stopping;
        self.set_online(online);

        // If server just came online, update last active time
        if !was_online && online {
            // TODO: move this somewhere else
            info!(target: "lazymc::monitor", "Server is now online");
            self.update_last_active_time();
            self.set_keep_online_until(Some(config.time.min_online_time));
        }

        // // If server just went offline, reset stopping state
        // // TODO: do this elsewhere
        // if stopping && was_online && !online {
        //     self.stopping.store(false, Ordering::Relaxed);
        // }

        if let Some(status) = status {
            // Update last active time if there are online players
            if status.players.online > 0 {
                self.update_last_active_time();
            }

            // Update last known players
            self.set_status(status);
        }
    }

    /// Check whether the server should now sleep.
    pub fn should_sleep(&self, config: &Config) -> bool {
        // TODO: when initating server start, set last active time!
        // TODO: do not initiate sleep when starting?
        // TODO: do not initiate sleep when already initiated (with timeout)

        // Don't sleep when keep online until isn't expired
        let keep_online = self
            .keep_online_until
            .lock()
            .unwrap()
            .map(|i| i >= Instant::now())
            .unwrap_or(false);
        if keep_online {
            trace!(target: "lazymc", "Not sleeping because of keep online");
            return false;
        }

        // Server must be online, and must not be starting
        if !self.online() || !self.starting() {
            return false;
        }

        // Never idle if players are online
        let players_online = self
            .status
            .lock()
            .unwrap()
            .as_ref()
            .map(|status| status.players.online > 0)
            .unwrap_or(false);
        if players_online {
            return false;
        }

        // Last active time must have passed sleep threshold
        if let Some(last_idle) = self.last_active.lock().unwrap().as_ref() {
            return last_idle.elapsed() >= Duration::from_secs(config.time.sleep_after as u64);
        }

        false
    }
}

/// Try to start the server.
///
/// Does not start if alreayd starting.
// TODO: move this into server state struct?
pub fn start_server(config: Arc<Config>, server: Arc<ServerState>) {
    // Ensure it is not starting yet
    if server.starting() {
        return;
    }

    // Update starting states
    // TODO: this may data race, use single atomic operation
    server.set_starting(true);
    server.update_last_active_time();

    // Spawn server in separate task
    tokio::spawn(invoke_server_command(config, server).map(|_| ()));
}

/// Invoke server command, store PID and wait for it to quit.
pub async fn invoke_server_command(
    config: Arc<Config>,
    state: Arc<ServerState>,
) -> Result<(), Box<dyn std::error::Error>> {
    // TODO: this doesn't properly handle quotes
    let args = config
        .server
        .command
        .split_terminator(" ")
        .collect::<Vec<_>>();

    // Build command
    let mut cmd = Command::new(args[0]);
    cmd.args(args.iter().skip(1));
    if let Some(ref dir) = config.server.directory {
        cmd.current_dir(dir);
    }
    cmd.kill_on_drop(true);

    info!(target: "lazymc", "Starting server...");
    let mut child = cmd.spawn()?;

    state.set_pid(Some(child.id().expect("unknown server PID")));

    let status = child.wait().await?;
    info!(target: "lazymc", "Server stopped (status: {})\n", status);

    // Reset online and starting state
    // TODO: also set this when returning early due to error
    state.set_pid(None);
    state.set_online(false);
    state.set_starting(false);
    state.stopping.store(false, Ordering::Relaxed);

    Ok(())
}

/// Stop server through RCON.
#[cfg(feature = "rcon")]
async fn stop_server_rcon(config: &Config, server: &ServerState) -> bool {
    use crate::mc::rcon::Rcon;

    // RCON must be enabled
    if !config.rcon.enabled {
        return false;
    }

    // RCON address
    let mut addr = config.server.address.clone();
    addr.set_port(config.rcon.port);
    let addr = addr.to_string();

    // Create RCON client
    let mut rcon = match Rcon::connect(&addr, &config.rcon.password).await {
        Ok(rcon) => rcon,
        Err(_) => {
            error!(target: "lazymc", "failed to create RCON client to sleep server");
            return false;
        }
    };

    // Invoke save-all
    if let Err(err) = rcon.cmd("save-all").await {
        error!(target: "lazymc", "failed to invoke save-all through RCON, ignoring: {}", err);
    }

    // Invoke stop
    if let Err(err) = rcon.cmd("stop").await {
        error!(target: "lazymc", "failed to invoke stop through RCON: {}", err);
    }

    // TODO: should we set this?
    server.set_online(false);
    server.set_keep_online_until(None);

    true
}

/// Stop server by sending SIGTERM signal.
///
/// Only works on Unix.
#[cfg(unix)]
fn stop_server_signal(server: &ServerState) -> bool {
    if let Some(pid) = *server.pid.lock().unwrap() {
        debug!(target: "lazymc", "Sending kill signal to server");
        crate::os::kill_gracefully(pid);

        // TODO: should we set this?
        server.set_online(false);
        server.set_keep_online_until(None);

        return true;
    }

    false
}
