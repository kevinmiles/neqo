// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use neqo_common::now;
use neqo_crypto::init;
//use neqo_transport::frame::StreamType;
use neqo_http3::{Http3Connection, Http3Event};
use neqo_transport::frame::StreamType;
use neqo_transport::{Connection, ConnectionEvent, Datagram, State};
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs, UdpSocket};
// use std::path::PathBuf;
use std::str::FromStr;
use std::string::ParseError;
use std::thread;
use std::time::{Duration, Instant};
use structopt::StructOpt;

#[derive(Debug, StructOpt, Clone)]
#[structopt(name = "neqo-interop", about = "A QUIC interop client.")]
struct Args {
    #[structopt(short = "p", long)]
    // Peers to include
    include: Vec<String>,

    #[structopt(short = "P", long)]
    exclude: Vec<String>,

    #[structopt(short = "t", long)]
    include_tests: Vec<String>,

    #[structopt(short = "T", long)]
    exclude_tests: Vec<String>,
}

trait Handler {
    fn handle(&mut self, client: &mut Connection) -> bool;
}

fn emit_packets(socket: &UdpSocket, out_dgrams: &Vec<Datagram>) {
    for d in out_dgrams {
        let sent = socket.send(&d[..]).expect("Error sending datagram");
        if sent != d.len() {
            eprintln!("Unable to send all {} bytes of datagram", d.len());
        }
    }
}

fn process_loop(
    nctx: &NetworkCtx,
    client: &mut Connection,
    handler: &mut Handler,
    timeout: &Duration,
) -> Result<neqo_transport::connection::State, String> {
    let buf = &mut [0u8; 2048];
    let mut in_dgrams = Vec::new();
    let start = Instant::now();

    loop {
        client.process_input(in_dgrams.drain(..), now());

        if let State::Closed(..) = client.state() {
            return Ok(client.state().clone());
        }

        let exiting = !handler.handle(client);
        let (out_dgrams, _timer) = client.process_output(now());
        emit_packets(&nctx.socket, &out_dgrams);

        if exiting {
            return Ok(client.state().clone());
        }

        let spent = Instant::now() - start;
        if spent > *timeout {
            return Err(String::from("Timed out"));
        }
        nctx.socket
            .set_read_timeout(Some(*timeout - spent))
            .expect("Read timeout");
        let sz = match nctx.socket.recv(&mut buf[..]) {
            Ok(sz) => sz,
            Err(e) => {
                return Err(String::from(match e.kind() {
                    std::io::ErrorKind::WouldBlock => "Timed out",
                    _ => "Read error",
                }));
            }
        };

        if sz == buf.len() {
            eprintln!("Received more than {} bytes", buf.len());
            continue;
        }
        if sz > 0 {
            in_dgrams.push(Datagram::new(
                nctx.remote_addr.clone(),
                nctx.local_addr.clone(),
                &buf[..sz],
            ));
        }
    }
}

struct PreConnectHandler {}
impl Handler for PreConnectHandler {
    fn handle(&mut self, client: &mut Connection) -> bool {
        match client.state() {
            State::Connected => false,
            State::Closing(..) => false,
            _ => true,
        }
    }
}

// HTTP/0.9 IMPLEMENTATION
#[derive(Default)]
struct H9Handler {
    rbytes: usize,
    rsfin: bool,
    streams: HashSet<u64>,
}

// This is a bit fancier than actually needed.
impl Handler for H9Handler {
    fn handle(&mut self, client: &mut Connection) -> bool {
        let mut data = vec![0; 4000];
        for event in client.events() {
            eprintln!("Event: {:?}", event);
            match event {
                ConnectionEvent::RecvStreamReadable { stream_id } => {
                    if !self.streams.contains(&stream_id) {
                        eprintln!("Data on unexpected stream: {}", stream_id);
                        return false;
                    }

                    let (sz, fin) = client
                        .stream_recv(stream_id, &mut data)
                        .expect("Read should succeed");
                    data.truncate(sz);
                    eprintln!("Length={}", sz);
                    self.rbytes += sz;
                    if fin {
                        eprintln!("<FIN[{}]>", stream_id);
                        client.close(0, "kthxbye!");
                        self.rsfin = true;
                        return false;
                    }
                }
                ConnectionEvent::SendStreamWritable { stream_id } => {
                    eprintln!("stream {} writable", stream_id)
                }
                _ => {
                    eprintln!("Unexpected event {:?}", event);
                }
            }
        }

        true
    }
}

// HTTP/3 IMPLEMENTATION
#[derive(Debug)]
struct Headers {
    pub h: Vec<(String, String)>,
}

// dragana: this is a very stupid parser.
// headers should be in form "[(something1, something2), (something3, something4)]"
impl FromStr for Headers {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut res = Headers { h: Vec::new() };
        let h1: Vec<&str> = s
            .trim_matches(|p| p == '[' || p == ']')
            .split(")")
            .collect();

        for h in h1 {
            let h2: Vec<&str> = h
                .trim_matches(|p| p == ',')
                .trim()
                .trim_matches(|p| p == '(' || p == ')')
                .split(",")
                .collect();

            if h2.len() == 2 {
                res.h
                    .push((h2[0].trim().to_string(), h2[1].trim().to_string()));
            }
        }

        Ok(res)
    }
}

struct H3Handler {
    streams: HashSet<u64>,
    h3: Http3Connection,
    host: String,
    path: String,
}

// TODO(ekr@rtfm.com): Figure out how to merge this.
fn process_loop_h3(
    nctx: &NetworkCtx,
    handler: &mut H3Handler,
    timeout: &Duration,
) -> Result<neqo_transport::connection::State, String> {
    let buf = &mut [0u8; 2048];
    let mut in_dgrams = Vec::new();
    let start = Instant::now();

    loop {
        handler.h3.conn().process_input(in_dgrams.drain(..), now());

        if let State::Closed(..) = handler.h3.conn().state() {
            return Ok(handler.h3.conn().state().clone());
        }

        let exiting = !handler.handle();
        let (out_dgrams, _timer) = handler.h3.conn().process_output(now());
        emit_packets(&nctx.socket, &out_dgrams);

        if exiting {
            return Ok(handler.h3.conn().state().clone());
        }

        let spent = Instant::now() - start;
        if spent > *timeout {
            return Err(String::from("Timed out"));
        }
        nctx.socket
            .set_read_timeout(Some(*timeout - spent))
            .expect("Read timeout");
        let sz = match nctx.socket.recv(&mut buf[..]) {
            Ok(sz) => sz,
            Err(e) => {
                return Err(String::from(match e.kind() {
                    std::io::ErrorKind::WouldBlock => "Timed out",
                    _ => "Read error",
                }));
            }
        };

        if sz == buf.len() {
            eprintln!("Received more than {} bytes", buf.len());
            continue;
        }
        if sz > 0 {
            in_dgrams.push(Datagram::new(
                nctx.remote_addr.clone(),
                nctx.local_addr.clone(),
                &buf[..sz],
            ));
        }
    }
}

// This is a bit fancier than actually needed.
impl H3Handler {
    fn handle(&mut self) -> bool {
        let mut data = vec![0; 4000];
        self.h3.process_http3();
        for event in self.h3.events() {
            match event {
                Http3Event::HeaderReady { stream_id } => {
                    if !self.streams.contains(&stream_id) {
                        println!("Data on unexpected stream: {}", stream_id);
                        return false;
                    }

                    let headers = self.h3.get_headers(stream_id);
                    println!("READ HEADERS[{}]: {:?}", stream_id, headers);
                }
                Http3Event::DataReadable { stream_id } => {
                    if !self.streams.contains(&stream_id) {
                        println!("Data on unexpected stream: {}", stream_id);
                        return false;
                    }

                    let (_sz, fin) = self
                        .h3
                        .read_data(stream_id, &mut data)
                        .expect("Read should succeed");
                    println!(
                        "READ[{}]: {}",
                        stream_id,
                        String::from_utf8(data.clone()).unwrap()
                    );
                    if fin {
                        println!("<FIN[{}]>", stream_id);
                        self.h3.close(0, "kthxbye!");
                        return false;
                    }
                }
                _ => {}
            }
        }

        true
    }
}

struct Peer {
    label: &'static str,
    host: &'static str,
    port: u16,
}

impl Peer {
    fn addr(&self) -> SocketAddr {
        self.to_socket_addrs()
            .expect("Remote address error")
            .next()
            .expect("No remote addresses")
    }

    fn bind(&self) -> SocketAddr {
        match self.addr() {
            SocketAddr::V4(..) => SocketAddr::new(IpAddr::V4(Ipv4Addr::from([0; 4])), 0),
            SocketAddr::V6(..) => SocketAddr::new(IpAddr::V6(Ipv6Addr::from([0; 16])), 0),
        }
    }

    fn test_enabled(&self, _test: &Test) -> bool {
        true
    }
}

impl ToSocketAddrs for Peer {
    type Iter = ::std::vec::IntoIter<SocketAddr>;
    fn to_socket_addrs(&self) -> ::std::io::Result<Self::Iter> {
        // This is idiotic.  There is no path from hostname: String to IpAddr.
        // And no means of controlling name resolution either.
        std::fmt::format(format_args!("{}:{}", self.host, self.port)).to_socket_addrs()
    }
}

#[derive(Debug)]
enum Test {
    Connect,
    H9,
    H3,
}

impl Test {
    fn alpn(&self) -> Vec<String> {
        match self {
            Test::H3 => vec![String::from("h3-20")],
            _ => vec![String::from("hq-20")],
        }
    }

    fn label(&self) -> String {
        String::from(match self {
            Test::Connect => "connect",
            Test::H9 => "h9",
            Test::H3 => "h3",
        })
    }
}

struct NetworkCtx {
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
    socket: UdpSocket,
}

fn test_connect(nctx: &NetworkCtx, test: &Test, peer: &Peer) -> Result<(Connection), String> {
    let mut client =
        Connection::new_client(peer.host, test.alpn(), nctx.local_addr, nctx.remote_addr)
            .expect("must succeed");
    // Temporary here to help out the type inference engine
    let mut h = PreConnectHandler {};
    let res = process_loop(nctx, &mut client, &mut h, &Duration::new(5, 0));

    let st = match res {
        Ok(st) => st,
        Err(e) => {
            return Err(format!("ERROR: {}", e));
        }
    };

    match st {
        State::Connected => Ok(client),
        _ => Err(format!("{:?}", st)),
    }
}

fn test_h9(nctx: &NetworkCtx, client: &mut Connection) -> Result<(), String> {
    let client_stream_id = client.stream_create(StreamType::BiDi).unwrap();
    let req: String = "GET /10\r\n".to_string();
    client
        .stream_send(client_stream_id, req.as_bytes())
        .unwrap();
    let mut hc = H9Handler::default();
    hc.streams.insert(client_stream_id);
    let res = process_loop(nctx, client, &mut hc, &Duration::new(5, 0));

    match res {
        Err(e) => {
            return Err(format!("ERROR: {}", e));
        }
        _ => {}
    };

    if hc.rbytes == 0 {
        return Err(String::from("Empty response"));
    }
    if !hc.rsfin {
        return Err(String::from("No FIN"));
    }
    Ok(())
}

fn test_h3(nctx: &NetworkCtx, peer: &Peer, client: Connection) -> Result<(), String> {
    let mut hc = H3Handler {
        streams: HashSet::new(),
        h3: Http3Connection::new(client, 128, 128),
        host: String::from(peer.host.clone()),
        path: String::from("/"),
    };

    let client_stream_id = hc
        .h3
        .fetch("GET", "https", &hc.host, &hc.path, &vec![])
        .unwrap();

    hc.streams.insert(client_stream_id);
    let res = process_loop_h3(nctx, &mut hc, &Duration::new(5, 0));
    match res {
        Err(e) => {
            return Err(format!("ERROR: {}", e));
        }
        _ => {}
    };

    Ok(())
}

fn run_test<'t>(peer: &Peer, test: &'t Test) -> (&'t Test, String) {
    let socket = UdpSocket::bind(peer.bind()).expect("Unable to bind UDP socket");
    socket.connect(&peer).expect("Unable to connect UDP socket");

    let local_addr = socket.local_addr().expect("Socket local address not bound");
    let remote_addr = peer.addr();

    let nctx = NetworkCtx {
        socket: socket,
        local_addr: local_addr,
        remote_addr: remote_addr,
    };

    let mut client = match test_connect(&nctx, test, peer) {
        Ok(client) => client,
        Err(e) => return (test, e),
    };

    let res = match test {
        Test::Connect => {
            return (test, String::from("OK"));
        }
        Test::H9 => test_h9(&nctx, &mut client),
        Test::H3 => test_h3(&nctx, peer, client),
    };

    match res {
        Ok(_) => {}
        Err(e) => return (test, e),
    }

    match test {
        _ => {
            return (test, String::from("OK"));
        }
    };
}

fn run_peer(args: &Args, peer: &'static Peer) -> Vec<(&'static Test, String)> {
    let mut results: Vec<(&'static Test, String)> = Vec::new();

    eprintln!("Running tests for {}", peer.label);

    let mut children = Vec::new();

    for test in &TESTS {
        if !peer.test_enabled(&test) {
            continue;
        }

        if args.include_tests.len() > 0 && !args.include_tests.contains(&test.label()) {
            continue;
        }
        if args.exclude_tests.contains(&test.label()) {
            continue;
        }

        let child = thread::spawn(move || run_test(peer, test));
        children.push((test, child));
    }

    for child in children {
        match child.1.join() {
            Ok(e) => {
                eprintln!("Test complete {:?}, {:?}", child.0, e);
                results.push(e)
            }
            Err(_) => {
                eprintln!("Thread crashed {:?}", child.0);
                results.push((child.0, String::from("CRASHED")));
            }
        }
    }

    println!("Tests for {} complete {:?}", peer.label, results);
    results
}

const PEERS: [Peer; 8] = [
    Peer {
        label: &"quant",
        host: &"quant.eggert.org",
        port: 4433,
    },
    Peer {
        label: &"quicly",
        host: "kazuhooku.com",
        port: 4433,
    },
    Peer {
        label: &"local",
        host: &"127.0.0.1",
        port: 4433,
    },
    Peer {
        label: &"applequic",
        host: &"192.168.203.142",
        port: 4433,
    },
    Peer {
        label: &"f5",
        host: &"208.85.208.226",
        port: 4433,
    },
    Peer {
        label: &"msft",
        host: &"quic.westus.cloudapp.azure.com",
        port: 4433,
    },
    Peer {
        label: &"mvfst",
        host: &"fb.mvfst.net",
        port: 4433,
    },
    Peer {
        label: &"google",
        host: &"quic.rocks",
        port: 4433,
    },
];

const TESTS: [Test; 3] = [Test::Connect, Test::H9, Test::H3];

fn main() {
    let _tests = vec![Test::Connect];

    let args = Args::from_args();
    init();

    let mut children = Vec::new();

    // Start all the children.
    for peer in &PEERS {
        if args.include.len() > 0 && !args.include.contains(&String::from(peer.label)) {
            continue;
        }
        if args.exclude.contains(&String::from(peer.label)) {
            continue;
        }

        let at = args.clone();
        let child = thread::spawn(move || run_peer(&at, &peer));
        children.push((peer, child));
    }

    // Now wait for them.
    for child in children {
        let res = child.1.join().unwrap();
        eprintln!("{} -> {:?}", child.0.label, res);
    }
}
