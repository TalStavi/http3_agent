use quinn::{Endpoint, ClientConfig};
use rustls::{Certificate, RootCertStore};
use apache_avro::{Schema, to_avro_datum};
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const TEST_DURATION: Duration = Duration::from_secs(10);
const CONCURRENT_REQUESTS: usize = 100;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load the self-signed certificate from the server
    let cert = std::fs::read("/Users/talstavi/Projects/http3_avro_json_logstash/server_cert.der")?;
    let cert = Certificate(cert);

    let mut roots = RootCertStore::empty();
    roots.add(&cert)?;

    // Set up the HTTP/3 client configuration
    let client_config = ClientConfig::with_root_certificates(roots);

    // Create the endpoint
    let mut endpoint = Endpoint::client("0.0.0.0:0".parse::<SocketAddr>()?)?;
    endpoint.set_default_client_config(client_config);

    // Create Avro schema
    let schema = Schema::parse_str(r#"
        {
            "type": "record",
            "name": "Message",
            "fields": [
                {"name": "id", "type": "string"},
                {"name": "content", "type": "string"}
            ]
        }
    "#)?;

    // Create Avro record
    let mut record = apache_avro::types::Record::new(&schema).unwrap();
    record.put("id", "12345");
    record.put("content", "Hello, Server!");

    // Serialize Avro record
    let avro_bytes = to_avro_datum(&schema, record)?;

    let successful_requests = Arc::new(AtomicUsize::new(0));
    let start_time = Instant::now();

    let mut handles = vec![];

    for _ in 0..CONCURRENT_REQUESTS {
        let endpoint = endpoint.clone();
        let avro_bytes = avro_bytes.clone();
        let successful_requests = Arc::clone(&successful_requests);

        let handle = tokio::spawn(async move {
            while start_time.elapsed() < TEST_DURATION {
                if let Ok(connecting) = endpoint.connect("127.0.0.1:4433".parse::<SocketAddr>().unwrap(), "localhost") {
                    if let Ok(conn) = connecting.await {
                        if let Ok((mut send, mut recv)) = conn.open_bi().await {
                            if send.write_all(&avro_bytes).await.is_ok() && send.finish().await.is_ok() {
                                if recv.read_to_end(1024 * 1024).await.is_ok() {
                                    successful_requests.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                    }
                }
            }
        });

        handles.push(handle);
    }

    for handle in handles {
        handle.await?;
    }

    let elapsed = start_time.elapsed();
    let total_requests = successful_requests.load(Ordering::Relaxed);
    let requests_per_second = total_requests as f64 / elapsed.as_secs_f64();

    println!("Test completed in {:?}", elapsed);
    println!("Total successful requests: {}", total_requests);
    println!("Requests per second: {:.2}", requests_per_second);

    Ok(())
}