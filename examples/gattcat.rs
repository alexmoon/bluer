//! Arbitrary GATT characteristic connections and listens.

use blez::{
    adv::{Advertisement, AdvertisementHandle},
    gatt::{
        local::{
            self, characteristic_control, Application, ApplicationHandle, CharacteristicControl,
            CharacteristicControlEvent, CharacteristicNotify, CharacteristicWrite, Service,
        },
        remote, CharacteristicFlags, CharacteristicReader, CharacteristicWriter, DescriptorFlags,
    },
    Adapter, AdapterEvent, Address, AddressType, Device, DeviceEvent, DeviceProperty, Session, SessionEvent,
    Uuid,
};
use bytes::BytesMut;
use clap::Clap;
use crossterm::{terminal, tty::IsTty};
use futures::{future, pin_mut, stream::SelectAll, FutureExt, StreamExt, TryFutureExt};
use libc::{STDIN_FILENO, STDOUT_FILENO};
use pretty_hex::{hex_write, HexConfig};
use std::{
    collections::HashSet,
    ffi::OsString,
    fmt::Display,
    iter,
    process::{exit, Command, Stdio},
    time::Duration,
};
use tab_pty_process::AsyncPtyMaster;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    select,
    time::{sleep, timeout},
};
use tokio_compat_02::IoCompat;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Clap)]
#[clap(
    name = "gattcat",
    about = "Arbitrary GATT characteristic connections and listens.",
    author = "Sebastian Urban <surban@surban.net>"
)]
struct Opts {
    #[clap(subcommand)]
    cmd: Cmd,
}

#[derive(Clap)]
enum Cmd {
    /// Perform service discovery.
    Discover(DiscoverOpts),
    /// Connect to remote device.
    Connect(ConnectOpts),
    /// Listen for connection from remote device.
    Listen(ListenOpts),
    /// Listen for connection from remote device and serve a program
    /// once a connection is established.
    Serve(ServeOpts),
}

async fn connect(device: &Device) -> Result<()> {
    if !device.is_connected().await? {
        let mut retries = 2;
        loop {
            match device.connect().and_then(|_| device.services()).await {
                Ok(_) => break,
                Err(_) if retries > 0 => {
                    retries -= 1;
                }
                Err(err) => return Err(err.into()),
            }
        }
    }
    Ok(())
}

fn char_flags_to_vec(f: &CharacteristicFlags) -> Vec<&'static str> {
    let mut v = Vec::new();
    if f.read {
        v.push("read");
    };
    if f.secure_read {
        v.push("secure read");
    };
    if f.encrypt_read {
        v.push("encrypt read");
    }
    if f.notify {
        v.push("notify");
    };
    if f.indicate {
        v.push("indicate");
    }
    if f.broadcast {
        v.push("broadcast");
    }
    if f.write {
        v.push("write")
    };
    if f.write_without_response {
        v.push("write without respone");
    }
    if f.reliable_write {
        v.push("reliable write");
    }
    if f.secure_write {
        v.push("secure write")
    }
    if f.encrypt_write {
        v.push("encrypt write")
    };
    if f.authenticated_signed_writes {
        v.push("authenticated signed writes");
    };
    if f.encrypt_authenticated_write {
        v.push("encrypt authenticated write");
    }
    if f.writable_auxiliaries {
        v.push("writable auxiliaries")
    }
    if f.authorize {
        v.push("authorize");
    }
    v
}

fn desc_flags_to_vec(f: &DescriptorFlags) -> Vec<&'static str> {
    let mut v = Vec::new();
    if f.read {
        v.push("read");
    };
    if f.secure_read {
        v.push("secure read");
    };
    if f.encrypt_read {
        v.push("encrypt read");
    }
    if f.write {
        v.push("write")
    };
    if f.secure_write {
        v.push("secure write")
    }
    if f.encrypt_write {
        v.push("encrypt write")
    };
    if f.encrypt_authenticated_write {
        v.push("encrypt authenticated write");
    }
    if f.authorize {
        v.push("authorize");
    }
    v
}

#[derive(Clap)]
struct DiscoverOpts {
    /// Address of local Bluetooth adapter to use.
    #[clap(long, short)]
    bind: Option<Address>,
    /// Timeout in seconds for discovering a device.
    #[clap(long, short, default_value = "15")]
    timeout: u64,
    /// Only show devices with public addresses.
    #[clap(long, short)]
    public_only: bool,
    /// Do not connect to discovered devices for GATT service discovery.
    #[clap(long, short)]
    no_connect: bool,
    /// Addresses of Bluetooth devices.
    /// If unspecified gattcat scans for devices.
    address: Vec<Address>,
}

impl DiscoverOpts {
    pub async fn perform(mut self) -> Result<()> {
        let (_session, adapter) = get_session_adapter(self.bind).await?;
        let mut discover = adapter.discover_devices().await?;
        let mut changes = SelectAll::new();
        let mut timeout = sleep(Duration::from_secs(self.timeout)).boxed();

        let mut addresses: HashSet<_> = self.address.drain(..).collect();
        let mut done = HashSet::new();
        let filter = !addresses.is_empty();

        loop {
            if filter && addresses.is_empty() {
                break;
            }
            let addr = select! {
                _ = &mut timeout => break,
                evt = discover.next() => {
                    match evt {
                        Some(AdapterEvent::DeviceAdded(addr)) => addr,
                        None => break,
                        _ => continue,
                    }
                },
                Some((addr, evt)) = changes.next() => {
                    match evt {
                        DeviceEvent::PropertyChanged(DeviceProperty::Rssi(_)) => addr,
                        _ => continue,
                    }
                }
            };
            if (filter && !addresses.contains(&addr)) || done.contains(&addr) {
                continue;
            }

            let dev = adapter.device(addr)?;
            if self.public_only && dev.address_type().await.unwrap_or_default() == AddressType::Random {
                continue;
            }
            if let Ok(Some(_)) = dev.rssi().await {
                // If RSSI is available, device is present.
                if let Err(err) = Self::handle_device(&dev, self.no_connect).await {
                    println!("  Error: {}", err);
                }
                let _ = dev.disconnect().await;
                println!();
                addresses.remove(&addr);
                done.insert(addr);
            } else {
                // Device may be cached, wait for RSSI to become available.
                if let Ok(events) = dev.events().await {
                    changes.push(events.map(move |evt| (addr, evt)).boxed());
                }
            }

            timeout = sleep(Duration::from_secs(self.timeout)).boxed();
        }

        Ok(())
    }

    async fn handle_device(dev: &Device, no_connect: bool) -> Result<()> {
        println!("Device {} [{}]", dev.address(), dev.address_type().await.unwrap_or_default());
        Self::print_device_info(&dev).await?;
        if !no_connect {
            Self::enumerate_services(&dev).await?;
        }

        Ok(())
    }

    async fn print_device_info(dev: &Device) -> Result<()> {
        Self::print_if_some(2, "Name", dev.name().await?, "");
        Self::print_if_some(2, "Icon", dev.icon().await?, "");
        Self::print_if_some(2, "Class", dev.class().await?, "");
        Self::print_if_some(2, "RSSI", dev.rssi().await?, "dBm");
        Self::print_if_some(2, "TX power", dev.tx_power().await?, "dBm");
        //Self::print_list(4, "Services", &dev.uuids().await?.unwrap_or_default());
        for (uuid, data) in dev.service_data().await?.unwrap_or_default() {
            let lines = iter::once(String::new()).chain(Self::to_hex(&data));
            Self::print_list(2, &format!("Service data {}", uuid), lines);
        }
        for (id, data) in dev.manufacturer_data().await?.unwrap_or_default() {
            let lines = iter::once(String::new()).chain(Self::to_hex(&data));
            Self::print_list(2, &format!("Manufacturer data 0x{:04x}", id), lines);
        }
        Ok(())
    }

    async fn enumerate_services(dev: &Device) -> Result<()> {
        match timeout(Duration::from_secs(20), connect(dev)).await {
            Ok(Ok(())) => (),
            Ok(Err(err)) => {
                println!("  Connect failed: {}", &err);
                return Ok(());
            }
            Err(_) => {
                println!("  Connect timed out");
                return Ok(());
            }
        }

        for service in dev.services().await? {
            if service.primary().await? {
                println!("  Primary service {}", service.uuid().await?);
            } else {
                println!("  Secondary service {}", service.uuid().await?);
            }

            let mut includes = Vec::new();
            for service_id in service.includes().await? {
                let included = dev.service(service_id).await?;
                includes.push(included.uuid().await?);
            }
            Self::print_list(4, "Includes", includes);

            for char in service.characteristics().await? {
                println!("    Characteristic {}", char.uuid().await?);
                let flags = char.flags().await?;
                Self::print_if_some(6, "Flags", Some(char_flags_to_vec(&flags).join(", ")), "");
                if flags.read {
                    if let Ok(value) = char.read().await {
                        Self::print_list(6, "Read", Self::to_hex(&value));
                    }
                }
                if flags.notify || flags.indicate {
                    if let Ok(ns) = char.notify().await {
                        pin_mut!(ns);
                        if let Ok(Some(value)) = timeout(Duration::from_secs(5), ns.next()).await {
                            Self::print_list(6, "Notify", Self::to_hex(&value));
                        }
                    }
                }

                for desc in char.descriptors().await? {
                    println!("      Descriptor {}", desc.uuid().await?);
                    if let Ok(flags) = desc.flags().await {
                        Self::print_if_some(8, "Flags", Some(desc_flags_to_vec(&flags).join(", ")), "");
                    }
                    if let Ok(value) = desc.read().await {
                        Self::print_list(8, "Read", Self::to_hex(&value));
                    }
                }
            }
        }
        Ok(())
    }

    fn print_if_some<T: Display>(indent: usize, label: &str, value: Option<T>, unit: &str) {
        if let Some(value) = value {
            println!("{}{:10}{} {}", " ".repeat(indent), label, value, unit);
        }
    }

    fn print_list<T: Display>(indent: usize, mut label: &str, values: impl IntoIterator<Item = T>) {
        for value in values {
            println!("{}{:10}{}", " ".repeat(indent), label, value);
            label = "";
        }
    }

    fn to_hex(v: &[u8]) -> Vec<String> {
        let cfg = HexConfig { title: false, ascii: true, width: 10, group: 0, chunk: 1 };
        let mut out = String::new();
        hex_write(&mut out, &v, cfg).unwrap();

        let mut lines = Vec::new();
        for line in out.lines() {
            let fields: Vec<_> = line.splitn(2, ':').collect();
            if fields.len() == 1 {
                lines.push(fields[0].to_string());
            } else {
                lines.push(fields[1].trim().to_string());
            }
        }
        lines
    }
}

#[derive(Clap)]
struct ConnectOpts {
    /// Address of local Bluetooth adapter to use.
    #[clap(long, short)]
    bind: Option<Address>,
    /// Switch the terminal into raw mode when input is a TTY.
    /// Use together with --pty when serving.
    #[clap(long, short)]
    raw: bool,
    /// Target GATT service.
    #[clap(long, short, default_value = "02091984-ecf2-4b12-8135-59f4b1d1904b")]
    service: Uuid,
    /// Target GATT characteristic.
    #[clap(long, short, default_value = "02091984-ecf2-4b12-8135-59f4b1d1904b")]
    characteristic: Uuid,
    /// Public Bluetooth address of target device.
    address: Address,
}

impl ConnectOpts {
    pub async fn perform(self) -> Result<()> {
        let (_session, adapter) = get_session_adapter(self.bind).await?;

        let mut disco = adapter.discover_devices().await?;
        let timeout = sleep(Duration::from_secs(15));
        pin_mut!(timeout);
        let char = loop {
            select! {
                Some(evt) = disco.next() => {
                    if let AdapterEvent::DeviceAdded(addr) = evt {
                        if addr == self.address {
                            let dev = adapter.device(addr)?;
                            if let Ok(Some(char)) = self.find_characteristic(&dev).await {
                                break char;
                            } else {
                                let _ = dev.disconnect().await;
                                let _ = adapter.remove_device(addr).await;
                            }
                        }
                    }
                }
                _ = &mut timeout => {
                    return Err("device, service or characteristic not found".into());
                }
            }
        };

        let rh = char.notify_io().await.ok();
        let wh = char.write_io().await.ok();

        if rh.is_none() && wh.is_none() {
            return Err("neither writing nor notify are supported".into());
        }

        let is_tty = std::io::stdin().is_tty();
        let in_raw = if is_tty && self.raw {
            terminal::enable_raw_mode()?;
            true
        } else {
            false
        };

        io_loop(rh, wh, tokio::io::stdin(), tokio::io::stdout(), true, is_tty, true).await?;

        if in_raw {
            terminal::disable_raw_mode()?;
        }

        Ok(())
    }

    async fn find_characteristic(&self, device: &Device) -> Result<Option<remote::Characteristic>> {
        if !device.is_connected().await? {
            let mut retries = 2;
            loop {
                match device.connect().await {
                    Ok(()) => break,
                    Err(_) if retries > 0 => {
                        retries -= 1;
                    }
                    Err(err) => return Err(err.into()),
                }
            }
        }

        for service in device.services().await? {
            if service.uuid().await? == self.service {
                for char in service.characteristics().await? {
                    if char.uuid().await? == self.characteristic {
                        return Ok(Some(char));
                    }
                }
            }
        }

        Ok(None)
    }
}

async fn io_loop(
    mut rh: Option<CharacteristicReader>, mut wh: Option<CharacteristicWriter>, pin: impl AsyncRead + Unpin,
    pout: impl AsyncWrite + Unpin, is_std: bool, rh_required: bool, pin_required: bool,
) -> Result<()> {
    let mut pin = Some(pin);
    let mut pout = Some(pout);

    while rh.is_some() || pin.is_some() {
        if rh_required && rh.is_none() {
            break;
        }
        if pin_required && pin.is_none() {
            break;
        }

        let mtu = match (&rh, &wh) {
            (Some(rh), _) => rh.mtu(),
            (_, Some(wh)) => wh.mtu(),
            _ => 100,
        };
        let mut recv_buf = BytesMut::with_capacity(mtu as usize);
        let mut pin_buf = BytesMut::with_capacity(mtu as usize);

        select! {
            res = async {
                match rh.as_mut() {
                    Some(rh) => rh.read_buf(&mut recv_buf).await,
                    None => future::pending().await,
                }
            } => {
                match res {
                    Ok(0) | Err(_) => {
                        log::debug!("remote read failed");
                        rh = None;
                        pout = None;
                        if is_std {
                            unsafe { libc::close(STDOUT_FILENO) };
                        }
                    },
                    Ok(_) => {
                        let pout = pout.as_mut().unwrap();
                        if pout.write_all(&recv_buf).await.is_err() || pout.flush().await.is_err() {
                            log::debug!("local output failed");
                            rh = None;
                        }
                    }
                }
            },
            res = async {
                match pin.as_mut() {
                    Some(pin) => pin.read_buf(&mut pin_buf).await,
                    None => future::pending().await,
                }
            } => {
                match res {
                    Ok(0) | Err(_) => {
                        log::debug!("local input failed");
                        wh = None;
                        pin = None;
                    },
                    Ok(_) => {
                        if wh.as_mut().unwrap().write_all(&pin_buf).await.is_err() {
                            log::debug!("remote write failed");
                            pin = None;
                            if is_std {
                                unsafe { libc::close(STDIN_FILENO) };
                            }
                        }
                    }
                }
            },
        }
    }

    Ok(())
}

#[derive(Clap)]
struct ListenOpts {
    /// Address of local Bluetooth adapter to use.
    #[clap(long, short)]
    bind: Option<Address>,
    /// Print listen and peer address to standard error.
    #[clap(long, short)]
    verbose: bool,
    /// Switch the terminal into raw mode when input is a TTY.
    #[clap(long)]
    raw: bool,
    /// Do not send LE advertisement packets.
    #[clap(long, short)]
    no_advertise: bool,
    /// GATT service to publish.
    #[clap(long, short, default_value = "02091984-ecf2-4b12-8135-59f4b1d1904b")]
    service: Uuid,
    /// GATT characteristic to publish.
    #[clap(long, short, default_value = "02091984-ecf2-4b12-8135-59f4b1d1904b")]
    characteristic: Uuid,
}

impl ListenOpts {
    pub async fn perform(self) -> Result<()> {
        let (_session, adapter) = get_session_adapter(self.bind).await?;
        let (_adv, _app, mut control) =
            make_app(&adapter, self.no_advertise, self.service, self.characteristic).await?;

        if self.verbose {
            println!("Serving on {}", adapter.address().await?);
        }

        let is_tty = std::io::stdin().is_tty();
        let in_raw = if is_tty && self.raw {
            terminal::enable_raw_mode()?;
            true
        } else {
            false
        };

        io_loop_serve(&mut control, None, None, tokio::io::stdin(), tokio::io::stdout(), true, true, true)
            .await?;

        if in_raw {
            terminal::disable_raw_mode()?;
        }

        Ok(())
    }
}

#[derive(Clap)]
struct ServeOpts {
    /// Address of local Bluetooth adapter to use.
    #[clap(long, short)]
    bind: Option<Address>,
    /// Print listen and peer address to standard error.
    #[clap(long, short)]
    verbose: bool,
    /// Do not send LE advertisement packets.
    #[clap(long, short)]
    no_advertise: bool,
    /// Exit after handling one connection.
    #[clap(long, short)]
    one_shot: bool,
    /// Allocate a pseudo-terminal (PTY) for the program.
    /// Use together with --raw when connecting.
    #[clap(long, short)]
    pty: bool,
    /// GATT service to publish.
    #[clap(long, short, default_value = "02091984-ecf2-4b12-8135-59f4b1d1904b")]
    service: Uuid,
    /// GATT characteristic to publish.
    #[clap(long, short, default_value = "02091984-ecf2-4b12-8135-59f4b1d1904b")]
    characteristic: Uuid,
    /// Program to execute once connection is established.
    command: OsString,
    /// Arguments to program.
    args: Vec<OsString>,
}

impl ServeOpts {
    pub async fn perform(self) -> Result<()> {
        use tab_pty_process::CommandExt;

        let (session, adapter) = get_session_adapter(self.bind).await?;

        if self.verbose {
            println!("Serving on {}", adapter.address().await?);
        }

        let adapter_name = adapter.name().to_string();
        let events = session.events().await?;
        tokio::spawn(async move {
            pin_mut!(events);
            loop {
                match events.next().await {
                    Some(SessionEvent::AdapterRemoved(name)) if name == adapter_name => break,
                    None => break,
                    _ => (),
                }
            }
            eprintln!("Adapter was disconnected or bluetoothd crashed");
            exit(3);
        });

        loop {
            let (_adv, _app, mut control) =
                make_app(&adapter, self.no_advertise, self.service, self.characteristic).await?;

            let mut rh = None;
            let mut wh = None;
            let mtu;

            match control.next().await {
                Some(CharacteristicControlEvent::Write(req)) => {
                    mtu = req.mtu();
                    rh = Some(req.accept()?);
                }
                Some(CharacteristicControlEvent::Notify(notifier)) => {
                    mtu = notifier.mtu();
                    wh = Some(notifier);
                }
                None => break,
            }

            if self.verbose {
                eprintln!("Connected with MTU {} bytes", mtu);
            }

            if self.pty {
                let ptymaster = AsyncPtyMaster::open()?;
                let mut cmd = Command::new(&self.command);
                cmd.args(&self.args);
                let child = match cmd.spawn_pty_async_raw(&ptymaster) {
                    Ok(child) => child,
                    Err(err) => {
                        eprintln!("Cannot execute {}: {}", &self.command.to_string_lossy(), &err);
                        continue;
                    }
                };

                let (pin, pout) = ptymaster.split();
                let pin = IoCompat::new(pin);
                let pout = IoCompat::new(pout);
                select! {
                    res = io_loop_serve(&mut control, rh, wh, pin, pout, false, true, false) => {
                        res?;
                        if self.verbose {
                            eprintln!("Connection terminated");
                        }
                    },
                    _ = child => {
                        if self.verbose {
                            eprintln!("Process exited");
                        }
                    },
                }
            } else {
                let mut cmd = tokio::process::Command::new(&self.command);
                cmd.args(&self.args);
                cmd.kill_on_drop(true);
                cmd.stdin(Stdio::piped());
                cmd.stdout(Stdio::piped());
                let mut child = match cmd.spawn() {
                    Ok(child) => child,
                    Err(err) => {
                        eprintln!("Cannot execute {}: {}", &self.command.to_string_lossy(), &err);
                        continue;
                    }
                };

                let pin = child.stdout.take().unwrap();
                let pout = child.stdin.take().unwrap();
                select! {
                    res = io_loop_serve(&mut control, rh, wh, pin, pout, false, true, false) => {
                        res?;
                        if self.verbose {
                            eprintln!("Connection terminated");
                        }
                    },
                    _ = child.wait() => {
                        if self.verbose {
                            eprintln!("Process exited");
                        }
                    },
                }
            }

            if self.one_shot {
                break;
            }
        }

        Ok(())
    }
}

async fn make_app(
    adapter: &Adapter, no_advertise: bool, service: Uuid, characteristic: Uuid,
) -> Result<(Option<AdvertisementHandle>, ApplicationHandle, CharacteristicControl)> {
    let le_advertisement = Advertisement {
        service_uuids: vec![service].into_iter().collect(),
        discoverable: Some(true),
        ..Default::default()
    };
    let adv = if !no_advertise { Some(adapter.advertise(le_advertisement).await?) } else { None };

    let (control, control_handle) = characteristic_control();
    let app = Application {
        services: vec![Service {
            uuid: service,
            primary: true,
            characteristics: vec![local::Characteristic {
                uuid: characteristic,
                write: Some(CharacteristicWrite {
                    write_without_response: true,
                    method: blez::gatt::local::CharacteristicWriteMethod::Io,
                    ..Default::default()
                }),
                notify: Some(CharacteristicNotify {
                    notify: true,
                    method: blez::gatt::local::CharacteristicNotifyMethod::Io,
                    ..Default::default()
                }),
                control_handle,
                ..Default::default()
            }],
            ..Default::default()
        }],
    };
    let app = adapter.serve_gatt_application(app).await?;

    Ok((adv, app, control))
}

async fn io_loop_serve(
    control: &mut CharacteristicControl, mut rh: Option<CharacteristicReader>,
    mut wh: Option<CharacteristicWriter>, pin: impl AsyncRead + Unpin, pout: impl AsyncWrite + Unpin,
    is_std: bool, rh_required: bool, pin_required: bool,
) -> Result<()> {
    let mut rh_closed = false;
    let mut wh_closed = false;

    let mut pin = Some(pin);
    let mut pout = Some(pout);

    while !rh_closed || pin.is_some() {
        if rh_required && rh_closed {
            break;
        }
        if pin_required && pin.is_none() {
            break;
        }
        if wh_closed {
            break;
        }

        let mtu = match (&rh, &wh) {
            (Some(rh), _) => rh.mtu(),
            (_, Some(wh)) => wh.mtu(),
            _ => 100,
        };
        let mut recv_buf = BytesMut::with_capacity(mtu as usize);
        let mut pin_buf = BytesMut::with_capacity(mtu as usize);

        let wh_present = wh.is_some();
        select! {
            evt = control.next() => {
                match evt {
                    Some(CharacteristicControlEvent::Write(req)) => {
                        rh = Some(req.accept()?);
                    },
                    Some(CharacteristicControlEvent::Notify(notifier)) => {
                        wh = Some(notifier);
                    },
                    None => break,
                }
            },
            res = async {
                match rh.as_mut() {
                    Some(rh) => rh.read_buf(&mut recv_buf).await,
                    None => future::pending().await,
                }
            } => {
                match res {
                    Ok(0) | Err(_) => {
                        log::debug!("remote read failed");
                        rh = None;
                        rh_closed = true;
                        pout = None;
                        if is_std {
                            unsafe { libc::close(STDOUT_FILENO) };
                        }
                    },
                    Ok(_) => {
                        let pout = pout.as_mut().unwrap();
                        if pout.write_all(&recv_buf).await.is_err() || pout.flush().await.is_err() {
                            log::debug!("local output failed");
                            rh = None;
                            rh_closed = true;
                        }
                    }
                }
            },
            res = async {
                match pin.as_mut() {
                    Some(pin) if wh_present => pin.read_buf(&mut pin_buf).await,
                    _ => future::pending().await,
                }
            } => {
                match res {
                    Ok(0) | Err(_) => {
                        log::debug!("local input failed");
                        wh = None;
                        pin = None;
                    },
                    Ok(_) => {
                        if wh.as_mut().unwrap().write_all(&pin_buf).await.is_err() {
                            log::debug!("remote write failed");
                            wh = None;
                            pin = None;
                            if is_std {
                                unsafe { libc::close(STDIN_FILENO) };
                            }
                        }
                    }
                }
            },
            res = async {
                match wh.as_mut() {
                    Some(wh) => wh.closed().await,
                    None => future::pending().await,
                }
            } => {
                res.unwrap();
                log::debug!("remote writer closed");
                wh = None;
                wh_closed = true;
            },
        }
    }

    Ok(())
}

async fn get_session_adapter(addr: Option<Address>) -> Result<(Session, Adapter)> {
    let session = blez::Session::new().await?;
    let adapter_names = session.adapter_names().await?;

    match addr {
        Some(addr) => {
            for adapter_name in adapter_names {
                let adapter = session.adapter(&adapter_name)?;
                if adapter.address().await? == addr {
                    adapter.set_powered(true).await?;
                    return Ok((session, adapter));
                }
            }
            Err("specified Bluetooth adapter not present".into())
        }
        None => {
            let adapter_name = adapter_names.first().ok_or("no Bluetooth adapter present")?;
            let adapter = session.adapter(&adapter_name)?;
            adapter.set_powered(true).await?;
            Ok((session, adapter))
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    use tokio_compat_02::FutureExt;

    env_logger::init();
    let opts: Opts = Opts::parse();
    let result = match opts.cmd {
        Cmd::Discover(d) => d.perform().await,
        Cmd::Connect(c) => c.perform().await,
        Cmd::Listen(l) => l.perform().await,
        Cmd::Serve(s) => s.perform().compat().await,
    };

    match result {
        Ok(_) => exit(0),
        Err(err) => {
            eprintln!("Error: {}", &err);
            exit(2);
        }
    }
}
