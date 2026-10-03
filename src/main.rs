use digital_kvm::{
    Result,
    config::{Config, UsbDevice},
    controller::{Controller, Transition},
    platform::{self, Event, EventSink, MonitorBackend, MonitorControl},
};
use serde_json::json;
use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, RecvTimeoutError},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

struct Args {
    command: String,
    config: PathBuf,
    log: Option<PathBuf>,
    input: Option<String>,
    dry_run: bool,
    background: bool,
    force: bool,
    seconds: Option<u64>,
    hold_ms: u64,
    service_name: String,
    output: Option<PathBuf>,
}
fn help() {
    println!(
        "digital-kvm 0.1.0\n\nCommands:\n  devices      List connected USB device identities\n  monitors     List monitor identities\n  validate     Check configuration without accessing hardware\n  learn-switch Observe a physical switch cycle and save its hub identity\n  status       Show configured USB presence and monitor input\n  probe        Check monitor access in the current process context\n  watch        Log USB changes without switching monitors\n  run          Switch monitor inputs on configured USB changes\n  set INPUT    Select a configured input (e.g. dp or usbc)\n  roundtrip    Select remote input, then automatically restore local\n\nOptions:\n  --config PATH       Configuration file (default: config.json beside executable)\n  --output PATH       Save learned configuration here instead of overwriting it\n  --log PATH          Append JSON events to a file (run/watch have a default log)\n  --dry-run           Log intended actions without sending monitor commands\n  --background        Suppress console output and detach the Windows console\n  --force             Force a set command; probe also tests a monitor write\n  --seconds N         Stop run/watch after N seconds; learn-switch timeout\n  --hold-ms N         Roundtrip dwell time (default 8000 ms)\n\nOptions may appear before or after the command. Ctrl+C stops foreground operation. Configuration IDs use decimal JSON numbers."
    );
}
fn arguments() -> Result<Args> {
    let mut items = env::args().skip(1);
    let executable = env::current_exe().map_err(|e| e.to_string())?;
    let mut args = Args {
        command: String::new(),
        config: executable
            .parent()
            .unwrap_or(Path::new("."))
            .join("config.json"),
        log: None,
        input: None,
        dry_run: false,
        background: false,
        force: false,
        seconds: None,
        hold_ms: 8000,
        service_name: "DigitalKvm".into(),
        output: None,
    };
    let mut had_arguments = false;
    let mut help_requested = false;
    while let Some(item) = items.next() {
        had_arguments = true;
        match item.as_str() {
            "--config" => args.config = PathBuf::from(items.next().ok_or("--config needs a path")?),
            "--output" => {
                args.output = Some(PathBuf::from(items.next().ok_or("--output needs a path")?))
            }
            "--service-name" => {
                args.service_name = items.next().ok_or("--service-name needs a name")?
            }
            "--log" => args.log = Some(PathBuf::from(items.next().ok_or("--log needs a path")?)),
            "--dry-run" => args.dry_run = true,
            "--background" => args.background = true,
            "--force" => args.force = true,
            "--seconds" => {
                args.seconds = Some(
                    items
                        .next()
                        .ok_or("--seconds needs a number")?
                        .parse()
                        .map_err(|_| "Invalid --seconds")?,
                )
            }
            "--hold-ms" => {
                args.hold_ms = items
                    .next()
                    .ok_or("--hold-ms needs a number")?
                    .parse()
                    .map_err(|_| "Invalid --hold-ms")?
            }
            "--help" | "-h" => help_requested = true,
            _ if args.command.is_empty() && !item.starts_with('-') => args.command = item,
            _ if args.command == "set" && args.input.is_none() && !item.starts_with('-') => {
                args.input = Some(item)
            }
            _ => return Err(format!("Unknown argument {item}")),
        }
    }
    if help_requested || !had_arguments {
        args.command = "help".into();
    }
    if args.command.is_empty() {
        return Err("An option requires a command. For example: digital-kvm set dp --force".into());
    }
    if args.hold_ms > 30000 && args.command != "restore-after" {
        return Err("--hold-ms must be at most 30000".into());
    }
    Ok(args)
}
fn print_json<T: serde::Serialize>(value: &T) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).map_err(|e| e.to_string())?
    );
    Ok(())
}
fn data_directory() -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(env::var_os("LOCALAPPDATA").unwrap_or_else(|| env::temp_dir().into()))
            .join("DigitalKvm")
    }
    #[cfg(target_os = "macos")]
    {
        PathBuf::from(env::var_os("HOME").unwrap_or_else(|| env::temp_dir().into()))
            .join("Library/Application Support/DigitalKvm")
    }
    #[cfg(target_os = "linux")]
    {
        env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env::var_os("HOME").unwrap_or_else(|| env::temp_dir().into()))
                    .join(".local/share")
            })
            .join("digital-kvm")
    }
}
struct Logger {
    path: Option<PathBuf>,
    file: Option<File>,
    quiet: bool,
}
impl Logger {
    fn new(path: Option<PathBuf>, quiet: bool) -> Result<Self> {
        if let Some(path) = &path
            && let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let file = path
            .as_ref()
            .map(|p| OpenOptions::new().create(true).append(true).open(p))
            .transpose()
            .map_err(|e| e.to_string())?;
        Ok(Self { path, file, quiet })
    }
    fn record(&mut self, event: &str, details: serde_json::Value) {
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let line = json!({"time_ms":time,"event":event,"details":details}).to_string();
        if !self.quiet {
            println!("{line}");
        }
        if let Some(file) = self.file.as_mut() {
            let _ = writeln!(file, "{line}");
            let _ = file.flush();
        }
        if let Some(path) = self.path.as_ref()
            && self
                .file
                .as_ref()
                .and_then(|f| f.metadata().ok())
                .is_some_and(|m| m.len() > 262144)
        {
            self.file.take();
            let backup = path.with_extension("log.1");
            let _ = fs::remove_file(&backup);
            let _ = fs::rename(path, &backup);
            self.file = OpenOptions::new().create(true).append(true).open(path).ok();
        }
    }
}

fn selected(devices: &[UsbDevice], config: &Config) -> bool {
    devices.iter().any(|d| config.usb.matches(d))
}

fn restore_guard(args: &Args) -> Result<std::process::Child> {
    use std::process::{Command, Stdio};
    let mut command = Command::new(env::current_exe().map_err(|e| e.to_string())?);
    command
        .args(["restore-after", "--config"])
        .arg(fs::canonicalize(&args.config).map_err(|e| e.to_string())?)
        .args(["--hold-ms", &(args.hold_ms + 2000).to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command
        .spawn()
        .map_err(|e| format!("Cannot arm automatic restore process: {e}"))
}
fn send_transition(
    config: &Config,
    action: Transition,
    dry_run: bool,
    generation: &AtomicU64,
    epoch: u64,
    log: &mut Logger,
) {
    let input = match action {
        Transition::ClaimLocal => &config.local_input,
        Transition::HandoffRemote => &config.remote_input,
    };
    if dry_run {
        log.record(
            "would_select_input",
            json!({"input":input,"transition":format!("{action:?}")}),
        );
        return;
    }
    let attempts = if action == Transition::ClaimLocal {
        config.arrival_attempts
    } else {
        1
    };
    let mut backend = None;
    for attempt in 0..attempts {
        if generation.load(Ordering::SeqCst) != epoch {
            log.record("cancelled_stale_command", json!({"input":input}));
            return;
        }
        if backend.is_none() {
            match MonitorBackend::new(&config.monitors) {
                Ok(value) => backend = Some(value),
                Err(error) => {
                    log.record(
                        "monitor_unavailable",
                        json!({"error":error,"attempt":attempt+1}),
                    );
                }
            }
        }
        let mut all_verified = backend.is_some();
        if let Some(backend) = backend.as_mut() {
            for monitor in &config.monitors {
                if generation.load(Ordering::SeqCst) != epoch {
                    return;
                }
                match backend.set_input(monitor, input) {
                    Ok(outcome) => {
                        all_verified &= outcome.verified;
                        log.record(
                            if outcome.verified {
                                "input_verified"
                            } else {
                                "input_command_sent_unverified"
                            },
                            json!(outcome),
                        );
                    }
                    Err(error) => {
                        all_verified = false;
                        log.record(
                            "input_command_failed",
                            json!({"input":input,"error":error,"attempt":attempt+1}),
                        );
                    }
                }
            }
        }
        if all_verified {
            return;
        }
        if attempt + 1 < attempts {
            thread::sleep(Duration::from_millis(config.retry_delay_ms));
        }
    }
}

fn run(config: &Config, args: &Args) -> Result<()> {
    let root = data_directory();
    fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    // A canonical config path separates independent configurations, while
    // aliases to the same file acquire the same OS-managed lock.
    let canonical = fs::canonicalize(&args.config).map_err(|e| e.to_string())?;
    let mut hash = 14695981039346656037u64;
    for byte in canonical.to_string_lossy().to_ascii_lowercase().bytes() {
        hash = (hash ^ byte as u64).wrapping_mul(1099511628211);
    }
    let _lock = platform::InstanceLock::acquire(&root.join(format!("{hash:016x}.lockfile")))?;
    let default_log = root.join(if args.command == "watch" {
        "watch.log"
    } else {
        "digital-kvm.log"
    });
    let mut log = Logger::new(
        Some(args.log.clone().unwrap_or(default_log)),
        args.background,
    )?;
    let (sender, receiver) = mpsc::sync_channel(64);
    let generation = Arc::new(AtomicU64::new(0));
    let sink = EventSink {
        sender,
        generation: generation.clone(),
        selector: if args.command == "watch" {
            None
        } else {
            Some(config.usb.clone())
        },
    };
    let _watcher = platform::Watcher::start(sink)?;
    #[cfg(windows)]
    if platform::windows_service::stop_requested() {
        return Ok(());
    }
    let mut devices = platform::usb_devices()?;
    let mut state = Controller::new(selected(&devices, config));
    log.record("started",json!({"pid":std::process::id(),"config":canonical,"usb_present":state.present(),"dry_run":args.dry_run || args.command=="watch","usb":config.usb,"log":log.path}));
    if args.background {
        platform::detach_console();
    }
    if args.command == "run" && config.reconcile_on_start && state.present() {
        send_transition(
            config,
            Transition::ClaimLocal,
            args.dry_run,
            &generation,
            generation.load(Ordering::SeqCst),
            &mut log,
        );
    }
    let stop_at = args
        .seconds
        .map(|s| Instant::now() + Duration::from_secs(s));
    let mut pending: Option<Instant> = None;
    let mut suspended = false;
    let mut resuming = false;
    loop {
        let deadline = match (pending, stop_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        };
        let event = if let Some(deadline) = deadline {
            receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        } else {
            receiver.recv().map_err(|_| RecvTimeoutError::Disconnected)
        };
        match event {
            Ok(Event::Stop) => break,
            Ok(Event::Suspend) => {
                suspended = true;
                resuming = false;
                pending = None;
                state.suspend();
                log.record("suspended", json!({}));
            }
            Ok(Event::Resume) => {
                suspended = false;
                resuming = true;
                pending = Some(Instant::now() + Duration::from_millis(config.resume_guard_ms));
                log.record(
                    "resume_guard",
                    json!({"milliseconds":config.resume_guard_ms}),
                );
            }
            Ok(Event::UsbChanged) => {
                if !suspended && !resuming {
                    pending = Some(Instant::now() + Duration::from_millis(config.debounce_ms));
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {
                if stop_at.is_some_and(|deadline| Instant::now() >= deadline) {
                    break;
                }
                if pending.is_none_or(|deadline| Instant::now() < deadline) {
                    continue;
                }
                pending = None;
                let epoch = generation.load(Ordering::SeqCst);
                let next = match platform::usb_devices() {
                    Ok(devices) => devices,
                    Err(error) => {
                        log.record("usb_enumeration_failed", json!({"error":error}));
                        continue;
                    }
                };
                if epoch != generation.load(Ordering::SeqCst) {
                    pending = Some(Instant::now() + Duration::from_millis(config.debounce_ms));
                    continue;
                }
                if args.command == "watch" {
                    for device in devices.iter().filter(|d| !next.contains(d)) {
                        log.record("usb_removed", json!(device));
                    }
                    for device in next.iter().filter(|d| !devices.contains(d)) {
                        log.record("usb_arrived", json!(device));
                    }
                }
                let present = selected(&next, config);
                let action = if resuming {
                    resuming = false;
                    state.resume(present)
                } else {
                    state.observe(present)
                };
                devices = next;
                if let Some(action) = action {
                    log.record(
                        "usb_transition",
                        json!({"present":present,"transition":format!("{action:?}")}),
                    );
                    if args.command == "run" {
                        send_transition(config, action, args.dry_run, &generation, epoch, &mut log);
                    }
                }
            }
        }
    }
    log.record("stopped", json!({}));
    Ok(())
}

fn execute() -> Result<()> {
    let args = arguments()?;
    #[cfg(windows)]
    if args.command == "service" || args.command == "service-probe" {
        return platform::windows_service::dispatch(&args.service_name, service_body);
    }
    match args.command.as_str() {
        "help" | "--help" | "-h" => {
            help();
            return Ok(());
        }
        "devices" => return print_json(&platform::usb_devices()?),
        "monitors" => return print_json(&platform::monitors()?),
        _ => {}
    }
    if args.command == "learn-switch" {
        return learn_switch(&args);
    }
    let config = Config::load(&args.config)?;
    match args.command.as_str() {
        "run" | "watch" => run(&config, &args),
        "validate" => print_json(&json!({"valid":true,"config":args.config})),
        "probe" => probe(&config, &args),
        "status" => {
            let devices = platform::usb_devices()?;
            let backend = MonitorBackend::new(&config.monitors)?;
            let statuses: Vec<_> = config
                .monitors
                .iter()
                .map(|c| json!({"configured":c,"ddc":platform::full_monitor_status(c)}))
                .collect();
            print_json(
                &json!({"usb_present":selected(&devices,&config),"matching_usb":devices.iter().filter(|d|config.usb.matches(d)).collect::<Vec<_>>(),"monitors":backend.identities(),"status":statuses}),
            )
        }
        "set" => {
            let input = args.input.as_ref().ok_or("set requires an input name")?;
            if config
                .monitors
                .iter()
                .any(|m| !m.inputs.contains_key(input))
            {
                return Err(format!("Input {input} is not configured on every monitor"));
            }
            if args.dry_run {
                return print_json(&json!({"would_select":input}));
            }
            let mut backend = MonitorBackend::new(&config.monitors)?;
            for monitor in &config.monitors {
                print_json(&backend.set_input_force(monitor, input, args.force)?)?;
            }
            Ok(())
        }
        "restore-after" => {
            thread::sleep(Duration::from_millis(args.hold_ms.min(60000)));
            let mut restored = false;
            for _ in 0..4 {
                if let Ok(mut backend) = MonitorBackend::new(&config.monitors) {
                    restored = true;
                    for monitor in &config.monitors {
                        restored &=
                            backend
                                .set_input(monitor, &config.local_input)
                                .is_ok_and(|outcome| {
                                    outcome.verified
                                        || !monitor.readback.contains_key(&config.local_input)
                                });
                    }
                }
                if restored {
                    break;
                }
                thread::sleep(Duration::from_millis(750));
            }
            if restored {
                Ok(())
            } else {
                Err("Automatic restore could not be verified".into())
            }
        }
        "roundtrip" => {
            if args.dry_run {
                return print_json(
                    &json!({"would_select":config.remote_input,"restore":config.local_input,"hold_ms":args.hold_ms}),
                );
            }
            let mut backend = MonitorBackend::new(&config.monitors)?;
            let mut guard = restore_guard(&args)?;
            // Always restore all configured monitors, even if a remote command
            // fails halfway through a group. This is a bounded diagnostic.
            let mut remote_error = None;
            for monitor in &config.monitors {
                match backend.set_input_force(monitor, &config.remote_input, true) {
                    Ok(outcome) => {
                        let _ = print_json(&outcome);
                    }
                    Err(error) => remote_error = Some(error),
                }
            }
            thread::sleep(Duration::from_millis(args.hold_ms));
            let mut remote_verified = true;
            for monitor in &config.monitors {
                let current = backend.current_input(monitor);
                remote_verified &= monitor
                    .readback
                    .get(&config.remote_input)
                    .is_some_and(|expected| current == Some(*expected));
                let _ = print_json(
                    &json!({"phase":"remote_input_readback","current_input":current,"expected":monitor.readback.get(&config.remote_input)}),
                );
            }
            let mut restore_error = None;
            for monitor in &config.monitors {
                match backend.set_input_force(monitor, &config.local_input, true) {
                    Ok(outcome) => {
                        let _ = print_json(&outcome);
                    }
                    Err(error) => restore_error = Some(error),
                }
            }
            let guard_status = guard
                .wait()
                .map_err(|e| format!("Cannot confirm restore process completion: {e}"))?;
            let verified = config.monitors.iter().all(|monitor| {
                monitor
                    .readback
                    .get(&config.local_input)
                    .is_some_and(|expected| backend.current_input(monitor) == Some(*expected))
            });
            let _ = print_json(
                &json!({"phase":"restored_local","verified":verified,"guard_succeeded":guard_status.success()}),
            );
            let local_verification_available = config
                .monitors
                .iter()
                .all(|m| m.readback.contains_key(&config.local_input));
            if !verified && local_verification_available {
                return Err(format!(
                    "Local input restore could not be verified; select {} with the monitor controls",
                    config.local_input
                ));
            }
            if let Some(error) = &restore_error {
                let _ =
                    print_json(&json!({"phase":"recovered_local","initial_restore_error":error}));
            }
            if let Some(error) = remote_error {
                return Err(format!(
                    "Remote input command failed; local input restored. {error}"
                ));
            }
            let remote_verification_available = config
                .monitors
                .iter()
                .all(|m| m.readback.contains_key(&config.remote_input));
            if !remote_verified && remote_verification_available {
                return Err("Remote input command was sent, but switching could not be verified; local input restored".into());
            }
            if restore_error.is_some() && !guard_status.success() {
                return Err("Both restore attempts failed; use the monitor controls to select the local input".into());
            }
            Ok(())
        }
        _ => Err(format!("Unknown command {}. Use --help.", args.command)),
    }
}
fn learn_switch(args: &Args) -> Result<()> {
    let mut config = Config::read(&args.config)?;
    config.usb.serial = None;
    config.usb.instance_contains = None;
    config.validate()?;
    let (sender, receiver) = mpsc::sync_channel(64);
    let generation = Arc::new(AtomicU64::new(0));
    let _watcher = platform::Watcher::start(EventSink {
        sender,
        generation,
        selector: None,
    })?;
    let initial: Vec<_> = platform::usb_devices()?
        .into_iter()
        .filter(|d| config.usb.matches(d))
        .collect();
    if initial.is_empty() {
        return Err(format!(
            "Connect the switch to this host first. No hub {:04X}:{:04X} is present.",
            config.usb.vendor_id, config.usb.product_id
        ));
    }
    println!(
        "Press the physical USB switch away from this host, wait a few seconds, then switch back. Monitor inputs will stay unchanged."
    );
    let deadline = Instant::now() + Duration::from_secs(args.seconds.unwrap_or(180).min(600));
    let mut departed = Vec::new();
    while Instant::now() < deadline {
        match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Event::UsbChanged) => {
                // Wait for the topology burst to settle; receive remains driven
                // by device events, not an idle polling loop.
                loop {
                    match receiver.recv_timeout(Duration::from_millis(config.debounce_ms)) {
                        Ok(Event::UsbChanged) => {}
                        Ok(Event::Stop) => return Err("Switch setup cancelled".into()),
                        Err(RecvTimeoutError::Timeout) => break,
                        Err(RecvTimeoutError::Disconnected) => {
                            return Err(
                                "USB notification channel closed during switch setup".into()
                            );
                        }
                        _ => {}
                    }
                }
                let current = platform::usb_devices()?;
                for device in &initial {
                    if !current.iter().any(|d| d.instance == device.instance)
                        && !departed.contains(device)
                    {
                        departed.push(device.clone());
                    }
                }
                if !departed.is_empty()
                    && departed
                        .iter()
                        .all(|d| current.iter().any(|c| c.instance == d.instance))
                {
                    let candidates: Vec<_> = departed
                        .iter()
                        .filter(|d| {
                            !departed.iter().any(|parent| {
                                parent.instance != d.instance
                                    && d.instance.starts_with(&(parent.instance.clone() + "/"))
                            })
                        })
                        .collect();
                    let selected = if let [device] = candidates.as_slice() {
                        *device
                    } else {
                        return Err(format!(
                            "More than one hub moved. Choose its instance from these observed candidates and set usb.instance_contains explicitly: {}",
                            serde_json::to_string_pretty(&candidates).map_err(|e| e.to_string())?
                        ));
                    };
                    config.usb.instance_contains = Some(selected.instance.clone());
                    config.validate()?;
                    let output = args.output.as_ref().unwrap_or(&args.config);
                    fs::write(
                        output,
                        serde_json::to_string_pretty(&config).map_err(|e| e.to_string())? + "\n",
                    )
                    .map_err(|e| e.to_string())?;
                    return print_json(&json!({"saved":output,"switch_hub":selected}));
                }
            }
            Ok(Event::Stop) => return Err("Switch setup cancelled".into()),
            Err(_) => break,
            _ => {}
        }
    }
    Err("No complete switch disconnect/connect cycle was observed before the timeout".into())
}
fn probe(config: &Config, args: &Args) -> Result<()> {
    let devices = platform::usb_devices()?;
    let mut backend = MonitorBackend::new(&config.monitors)?;
    let identities = backend.identities();
    let mut results = Vec::new();
    let mut reachable = true;
    let input = if selected(&devices, config) {
        &config.local_input
    } else {
        &config.remote_input
    };
    for monitor in &config.monitors {
        let matches = identities.iter().filter(|id| id.matches(monitor)).count();
        let current = backend.current_input(monitor);
        let command = if args.force {
            Some(backend.set_input_force(monitor, input, true))
        } else {
            None
        };
        let works = matches == 1
            && if let Some(result) = &command {
                result.is_ok()
            } else {
                current.is_some()
            };
        reachable &= works;
        results.push(
            json!({"matches":matches,"current_input":current,"command":command,"reachable":works}),
        );
    }
    let result = json!({"pid":std::process::id(),"reachable":reachable,"usb_present":selected(&devices,config),"monitors":results});
    if let Some(path) = &args.log {
        let mut logger = Logger::new(Some(path.clone()), true)?;
        logger.record("monitor_probe", result.clone());
    }
    print_json(&result)?;
    if reachable {
        Ok(())
    } else {
        Err("Monitor control could not be demonstrated in this process context".into())
    }
}
#[cfg(windows)]
fn service_body() -> Result<()> {
    let mut args = arguments()?;
    args.background = true;
    let result = (|| {
        let config = Config::load(&args.config)?;
        if args.command == "service-probe" {
            return probe(&config, &args);
        }
        args.command = "run".into();
        run(&config, &args)
    })();
    if let Err(error) = &result
        && let Some(path) = &args.log
        && let Ok(mut logger) = Logger::new(Some(path.clone()), true)
    {
        logger.record("service_failed", json!({"error":error}));
    }
    result
}
fn main() {
    if let Err(error) = execute() {
        let _ = writeln!(io::stderr(), "digital-kvm: {error}");
        std::process::exit(1);
    }
}
