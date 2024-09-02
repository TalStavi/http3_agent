use quinn::{Endpoint, ClientConfig, ReadExactError, ConnectError};
use rustls::{Certificate, RootCertStore};
use serde_json::{json, Value};
use rmp_serde::to_vec;
use rmp_serde::encode::Error as RmpEncodeError;
use std::net::SocketAddr;
use rand::{thread_rng, Rng};
use chrono::Utc;
use clap::Parser;
use tokio::time::{Duration, Instant};
use futures::future::join_all;
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};
use bytes::{BytesMut, BufMut};
use thiserror::Error;
use std::net::AddrParseError;

#[derive(Error, Debug)]
enum ClientError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Quinn connection error: {0}")]
    QuinnConnection(#[from] quinn::ConnectionError),
    #[error("Quinn write error: {0}")]
    QuinnWrite(#[from] quinn::WriteError),
    #[error("Quinn read error: {0}")]
    QuinnRead(#[from] quinn::ReadError),
    #[error("MessagePack decode error: {0}")]
    MessagePackDecode(#[from] rmp_serde::decode::Error),
    #[error("MessagePack encode error: {0}")]
    MessagePackEncode(#[from] RmpEncodeError),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("TLS error: {0}")]
    Tls(#[from] rustls::Error),
    #[error("Read exact error: {0}")]
    ReadExact(#[from] ReadExactError),
    #[error("Connect error: {0}")]
    Connect(#[from] ConnectError),
    #[error("Address parse error: {0}")]
    AddrParse(#[from] AddrParseError),
    #[error("Connection pool exhausted")]
    PoolExhausted,
    #[error("Max retries exceeded")]
    MaxRetriesExceeded,
    #[error("Unexpected server response")]
    UnexpectedResponse,
}

#[derive(Parser, Debug, Clone)]
#[clap(author, version, about, long_about = None)]
struct Args {
    #[clap(short, long, default_value = "127.0.0.1:4433")]
    server_addr: String,
    #[clap(short, long, default_value = "server_cert.der")]
    cert_path: String,
    #[clap(short, long)]
    debug: bool,
    #[clap(long)]
    stress_test: bool,
    #[clap(long, default_value = "200")]
    concurrent_connections: usize,
    #[clap(long, default_value = "50")]
    batch_size: usize,
    #[clap(long, default_value = "20")]
    stress_test_duration: u64,
    #[clap(long, default_value = "20")]
    json_pool_size: usize,
    #[clap(long, default_value = "10")]
    connection_pool_size: usize,
}

#[derive(Default)]
struct StressTestMetrics {
    requests_sent: usize,
    jsons_sent: usize,
    total_msgpack_size: usize,
    successful_responses: usize,
    failed_responses: usize,
    errors: Vec<String>,
}

struct ConnectionPool {
    connections: Vec<quinn::Connection>,
    semaphore: Arc<Semaphore>,
}

impl ConnectionPool {
    async fn new(endpoint: &Endpoint, server_addr: SocketAddr, size: usize) -> Result<Self, ClientError> {
        let mut connections = Vec::with_capacity(size);
        for _ in 0..size {
            let conn = endpoint.connect(server_addr, "localhost")?.await?;
            connections.push(conn);
        }
        Ok(Self {
            connections,
            semaphore: Arc::new(Semaphore::new(size)),
        })
    }

    async fn get_connection(&self) -> Result<quinn::Connection, ClientError> {
        let _permit = self.semaphore.acquire().await.map_err(|_| ClientError::PoolExhausted)?;
        Ok(self.connections[thread_rng().gen_range(0..self.connections.len())].clone())
    }
}

#[tokio::main]
async fn main() -> Result<(), ClientError> {
    let args = Args::parse();

    let cert = std::fs::read(&args.cert_path)?;
    let cert = Certificate(cert);
    let client_config = create_client_config(cert)?;

    let mut endpoint = Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(client_config);

    let server_addr: SocketAddr = args.server_addr.parse().expect("Valid address");

    if args.stress_test {
        run_stress_test(&endpoint, server_addr, args).await?;
    } else {
        run_single_request(&endpoint, server_addr, &args).await?;
    }

    Ok(())
}

async fn run_stress_test(endpoint: &Endpoint, server_addr: SocketAddr, args: Args) -> Result<(), ClientError> {
    println!("Starting stress test...");
    
    let start_time = Instant::now();
    let duration = Duration::from_secs(args.stress_test_duration);
    let metrics = Arc::new(Mutex::new(StressTestMetrics::default()));

    let json_pool = Arc::new(generate_json_pool(args.json_pool_size));

    let pool = Arc::new(ConnectionPool::new(endpoint, server_addr, args.connection_pool_size).await?);

    let mut handles = Vec::new();
    for _ in 0..args.concurrent_connections {
        let metrics_clone = Arc::clone(&metrics);
        let args_clone = args.clone();
        let json_pool_clone = Arc::clone(&json_pool);
        let pool_clone = Arc::clone(&pool);
        handles.push(tokio::spawn(async move {
            while start_time.elapsed() < duration {
                match send_batch_with_retry(&pool_clone, &metrics_clone, &args_clone, &json_pool_clone).await {
                    Ok(_) => {},
                    Err(e) => {
                        let mut metrics = metrics_clone.lock().await;
                        metrics.errors.push(format!("Batch error: {:?}", e));
                    }
                }
            }
        }));
    }

    join_all(handles).await;

    let elapsed = start_time.elapsed();
    let metrics = metrics.lock().await;
    
    println!("Stress test completed in {:.2} seconds", elapsed.as_secs_f64());
    println!("Requests sent: {}", metrics.requests_sent);
    println!("JSONs sent: {}", metrics.jsons_sent);
    println!("Average msgpack size per batch: {:.2} bytes", metrics.total_msgpack_size as f64 / metrics.requests_sent as f64);
    println!("Successful responses: {}", metrics.successful_responses);
    println!("Failed responses: {}", metrics.failed_responses);
    println!("Requests per second: {:.2}", metrics.requests_sent as f64 / elapsed.as_secs_f64());
    println!("JSONs per second: {:.2}", metrics.jsons_sent as f64 / elapsed.as_secs_f64());
    println!("Errors encountered: {}", metrics.errors.len());
    for error in &metrics.errors {
        println!("Error: {}", error);
    }

    Ok(())
}

async fn send_batch_with_retry(
    pool: &Arc<ConnectionPool>,
    metrics: &Arc<Mutex<StressTestMetrics>>,
    args: &Args,
    json_pool: &[Value],
) -> Result<(), ClientError> {
    const MAX_RETRIES: usize = 3;
    let mut retries = 0;

    while retries < MAX_RETRIES {
        let connection = pool.get_connection().await?;
        match send_batch(&connection, metrics, args, json_pool).await {
            Ok(_) => return Ok(()),
            Err(ClientError::QuinnWrite(quinn::WriteError::Stopped(_))) => {
                retries += 1;
                if retries < MAX_RETRIES {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            Err(e) => return Err(e),
        }
    }

    Err(ClientError::MaxRetriesExceeded)
}

async fn send_batch(
    connection: &quinn::Connection,
    metrics: &Arc<Mutex<StressTestMetrics>>,
    args: &Args,
    json_pool: &[Value],
) -> Result<(), ClientError> {
    let (mut send, mut recv) = connection.open_bi().await?;

    let mut batch = Vec::new();
    for _ in 0..args.batch_size {
        let process_data = select_random_json(json_pool);
        batch.push(process_data);
    }

    let batch_msgpack = to_vec(&batch)?;
    send_length_prefixed_message(&mut send, &batch_msgpack).await?;

    if args.debug {
        println!("Sent batch of {} messages", args.batch_size);
    }

    let response = read_ack(&mut recv).await?;

    let mut metrics = metrics.lock().await;
    metrics.requests_sent += 1;
    metrics.jsons_sent += args.batch_size;
    metrics.total_msgpack_size += batch_msgpack.len();

    if response == "ACK" {
        metrics.successful_responses += 1;
        if args.debug {
            println!("Received acknowledgment from server.");
        }
    } else {
        metrics.failed_responses += 1;
        if args.debug {
            println!("Received unexpected response from server: {}", response);
        }
    }

    Ok(())
}

async fn run_single_request(endpoint: &Endpoint, server_addr: SocketAddr, _args: &Args) -> Result<(), ClientError> {
    let connection = endpoint.connect(server_addr, "localhost")?.await?;
    println!("Connected to {}", server_addr);

    let (mut send, mut recv) = connection.open_bi().await?;

    let process_data = generate_fake_process_data();
    let data_to_send = vec![process_data]; // Wrap in an array
    let msgpack_data = to_vec(&data_to_send)?;
    send_length_prefixed_message(&mut send, &msgpack_data).await?;

    println!("Sent data: {}", serde_json::to_string_pretty(&data_to_send)?);

    let response = read_ack(&mut recv).await?;

    if response == "ACK" {
        println!("Received acknowledgment from server.");
    } else {
        println!("Received unexpected response from server: {}", response);
    }

    Ok(())
}

fn create_client_config(server_cert: Certificate) -> Result<ClientConfig, ClientError> {
    let mut roots = RootCertStore::empty();
    roots.add(&server_cert)?;
    
    let client_config = ClientConfig::with_root_certificates(roots);
    
    Ok(client_config)
}

fn generate_json_pool(pool_size: usize) -> Vec<Value> {
    (0..pool_size).map(|_| generate_fake_process_data()).collect()
}

fn select_random_json(json_pool: &[Value]) -> Value {
    let mut rng = thread_rng();
    json_pool[rng.gen_range(0..json_pool.len())].clone()
}

async fn send_length_prefixed_message(send: &mut quinn::SendStream, data: &[u8]) -> Result<(), ClientError> {
    let length = data.len() as u32;
    let length_bytes = length.to_be_bytes();
    
    let mut message = BytesMut::with_capacity(4 + data.len());
    message.put_slice(&length_bytes);
    message.put_slice(data);

    send.write_all(&message).await?;
    send.finish().await?;
    Ok(())
}

async fn read_ack(recv: &mut quinn::RecvStream) -> Result<String, ClientError> {
    let mut buf = [0u8; 4]; // ACK is 3 bytes, but we read 4 to include the msgpack string header
    recv.read_exact(&mut buf).await?;
    
    if &buf[1..] == b"ACK" {
        Ok("ACK".to_string())
    } else {
        Err(ClientError::UnexpectedResponse)
    }
}

fn generate_fake_process_data() -> Value {
    let mut rng = thread_rng();
    
    let process_names = ["chrome.exe", "firefox.exe", "notepad.exe", "explorer.exe"];
    let program_folders = ["Google", "Mozilla", "Windows NT", "Internet Explorer"];
    let program_names = ["chrome", "firefox", "notepad", "iexplore"];
    let user_names = ["ADMIN", "JOHN", "JANE", "GUEST"];
    let integrity_levels = ["Low", "Medium", "High", "System"];

    json!({
        "event_type": "process_creation",
        "timestamp": Utc::now().to_rfc3339(),
        "process": {
            "pid": rng.gen_range(1000..10000),
            "name": process_names[rng.gen_range(0..process_names.len())],
            "path": format!("C:\\Program Files\\{}\\{}.exe", 
                            program_folders[rng.gen_range(0..program_folders.len())],
                            program_names[rng.gen_range(0..program_names.len())]),
            "command_line": format!("{}.exe --flag1 --flag2=\"some value\"", 
                                    program_names[rng.gen_range(0..program_names.len())]),
            "user": format!("USER-{}", user_names[rng.gen_range(0..user_names.len())]),
            "integrity_level": integrity_levels[rng.gen_range(0..integrity_levels.len())],
        },
        "parent_process": {
            "pid": rng.gen_range(1..1000),
            "name": "explorer.exe",
            "path": "C:\\Windows\\explorer.exe",
        },
        "network": {
            "local_ip": format!("192.168.1.{}", rng.gen_range(1..255)),
            "local_port": rng.gen_range(1024..65535),
            "remote_ip": format!("{}.{}.{}.{}", rng.gen_range(1..255), rng.gen_range(1..255), rng.gen_range(1..255), rng.gen_range(1..255)),
            "remote_port": rng.gen_range(1..65535),
        },
        "file_system": {
            "files_accessed": [
                format!("C:\\Users\\{}\\Documents\\file{}.txt", user_names[rng.gen_range(0..user_names.len())], rng.gen_range(1..10)),
                format!("C:\\Program Files\\file{}.dll", rng.gen_range(1..10)),
            ],
        },
        "registry": {
            "keys_accessed": [
                format!("HKEY_LOCAL_MACHINE\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Run\\Program{}", rng.gen_range(1..10)),
                format!("HKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\User Shell Folders\\Desktop"),
            ],
        },
    })
}
