# QUIC Logstash Client

This project implements a high-performance QUIC client that sends JSON messages to a QUIC server (such as the QUIC Logstash Server). It's designed for efficient, secure, and reliable data transmission in logging and metrics collection systems.

## Features

- QUIC protocol for fast, secure communication
- Support for both single requests and stress testing
- Configurable batch sizes and concurrent connections
- Connection pooling for improved performance
- Automatic retries on connection errors
- Generation of realistic process data for testing

## Dependencies

This client relies on several Rust crates:

- `quinn`: Implements the QUIC protocol
- `rustls`: Provides TLS functionality
- `serde_json`: Processes JSON data
- `rmp_serde`: Handles MessagePack serialization
- `tokio`: Powers the asynchronous runtime
- `clap`: Parses command-line arguments
- `chrono`: Handles date and time operations
- `rand`: Generates random data for testing
- `bytes`: Helps with byte buffer management
- `thiserror`: Provides convenient error handling

## Main Components

1. **Connection Pool**: Manages a pool of QUIC connections for efficient reuse.
2. **Stress Test**: Simulates high load by sending multiple concurrent requests.
3. **Batch Sending**: Groups multiple JSON messages into a single request.
4. **Error Handling**: Comprehensive error types and handling for robustness.
5. **Metrics Collection**: Tracks various performance metrics during stress tests.

## Key Functions

- `main`: Entry point, sets up the client and runs tests
- `run_stress_test`: Executes a stress test with multiple concurrent connections
- `send_batch_with_retry`: Sends a batch of messages with automatic retries
- `generate_fake_process_data`: Creates realistic process data for testing
- `send_length_prefixed_message`: Sends messages with length prefixes
- `read_ack`: Reads acknowledgments from the server

## Usage

```sh
cargo run -- [OPTIONS]

Options:
  -s, --server-addr <SERVER_ADDR>                  [default: 127.0.0.1:4433]
  -c, --cert-path <CERT_PATH>                      [default: server_cert.der]
  -d, --debug
      --stress-test
      --concurrent-connections <CONCURRENT_CONNECTIONS>  [default: 200]
      --batch-size <BATCH_SIZE>                    [default: 50]
      --stress-test-duration <STRESS_TEST_DURATION>  [default: 20]
      --json-pool-size <JSON_POOL_SIZE>            [default: 20]
      --connection-pool-size <CONNECTION_POOL_SIZE>  [default: 10]
  -h, --help                                       Print help
  -V, --version                                    Print version
```

## Running a Test

1. To run a single request:
   ```
   cargo run -- -s 127.0.0.1:4433 -c path/to/server_cert.der
   ```

2. To run a stress test:
   ```
   cargo run -- -s 127.0.0.1:4433 -c path/to/server_cert.der --stress-test --concurrent-connections 100 --batch-size 25 --stress-test-duration 30
   ```

Make sure the server certificate (`server_cert.der`) is in the specified path.

## Stress Test Metrics

The stress test provides the following metrics:

- Total requests sent
- Total JSONs sent
- Average MessagePack size per batch
- Successful and failed responses
- Requests and JSONs per second
- Errors encountered

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## License

[MIT License](LICENSE)
