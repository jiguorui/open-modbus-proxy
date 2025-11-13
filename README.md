# Modbus TCP Proxy

A high-performance, asynchronous Modbus TCP proxy server built with Rust and Tokio. This proxy forwards Modbus TCP requests from clients to a target Modbus server with Unit-ID passthrough, providing robust connection management and comprehensive metrics.

## Features

- **Unit-ID Passthrough**: Forwards client Unit-IDs transparently to the target server
- **Connection Management**: Automatic reconnection to target server with connection state tracking
- **Configurable Timeouts**: Backend timeout protection prevents indefinite hanging on slow/unresponsive targets
- **Comprehensive Metrics**: Built-in metrics tracking with JSON snapshot output
- **Structured Logging**: Separate log files for regular logs and metrics with daily rotation
- **Strict Mode**: Configurable error mapping (GatewayPathUnavailable vs ServerDeviceFailure)
- **High Performance**: Built on Tokio for async I/O handling
- **Concurrent Connections**: Supports multiple simultaneous client connections

## Quick Start

### Prerequisites

- Rust 1.70+
- Cargo

### Installation

```bash
git clone <repository-url>
cd modbus-proxy
cargo build --release
```

### Running the Proxy

#### Basic Usage

```bash
# Forward Modbus requests to a target server
cargo run --release -- --target 192.168.1.100:502

# Listen on a custom address
cargo run --release -- --target 192.168.1.100:502 --listen 127.0.0.1:8080
```

#### With Custom Log Directory

```bash
# Store logs in a specific directory
cargo run --release -- --target 192.168.1.100:502 --log-dir /var/log/modbus-proxy
```

#### Non-Strict Mode

```bash
# Use ServerDeviceFailure instead of GatewayPathUnavailable for errors
cargo run --release -- --target 192.168.1.100:502 --strict false
```

#### Disable Console Logging

```bash
# Run proxy without console output (logs still written to files)
cargo run --release -- --target 192.168.1.100:502 --no-console
```

#### Metrics JSON Output

```bash
# Output current metrics as JSON and exit (useful for monitoring scripts)
cargo run --release -- --metrics-json
```

### Docker Usage

```bash
docker build -t modbus-proxy .
docker run -p 5020:5020 modbus-proxy --target 192.168.1.100:502
```

## Architecture

```
┌─────────────────┐
│ Modbus Client 1 │
└────────┬────────┘
         │
         │ Connects to
         │
┌────────▼────────┐
│ Modbus Client 2 │
└────────┬────────┘
         │
         │ Connects to
         │
┌────────▼────────┐
│ Modbus TCP Proxy│◄─────────────────┐
│ (Port 5020)     │                  │
└────────┬────────┘                  │
         │                           │
         │ Forwards requests         │
         │ with Unit-ID passthrough  │
         │                           │
┌────────▼────────┐                  │
│ Target Modbus   │                  │
│ Server          │                  │
│ (Port 502)      │                  │
└─────────────────┘                  │
                                      │
┌─────────────────┐                  │
│ Modbus Client N │                  │
└────────┬────────┘                  │
         │                           │
         └───────────────────────────┘
               Connects to
```

## Configuration

### Command-Line Arguments

```
Usage: modbus-proxy [OPTIONS]

Options:
  -t, --target <ADDR>           Target Modbus server address (e.g., 192.168.1.100:502)
  -l, --listen <ADDR>           Listen address for the proxy [default: 0.0.0.0:5020]
      --strict <BOOL>           Strict mode: map errors to GatewayPathUnavailable [default: true]
      --log-dir <DIR>           Log directory for file output [default: ./logs]
      --backend-timeout <SECS>  Backend timeout in seconds for upstream operations [default: 30]
      --metrics-json            Output JSON metrics snapshot and exit
      --no-console              Disable console logging (only write to log files)
  -h, --help                    Print help
  -V, --version                 Print version
```

### Environment Variables

- `RUST_LOG`: Logging level (e.g., "info", "debug", "error")

### Default Settings

- Listen Address: `0.0.0.0:5020`
- Strict Mode: `true`
- Log Directory: `./logs`
- Backend Timeout: `30 seconds`
- Metrics Interval: 30 seconds

## Supported Function Codes

The proxy forwards all standard Modbus function codes:

- `0x01`: Read Coils
- `0x02`: Read Discrete Inputs
- `0x03`: Read Holding Registers
- `0x04`: Read Input Registers
- `0x05`: Write Single Coil
- `0x06`: Write Single Register
- `0x0F`: Write Multiple Coils
- `0x10`: Write Multiple Registers

Unsupported function codes return `IllegalFunction` exception.

## Metrics

The proxy tracks comprehensive metrics including:

- **Connection Metrics**:
  - Total client connections
  - Active client connections
  - Total target server connections
  - Target server connection status

- **Request Metrics** (per function code):
  - Read Coils count
  - Read Discrete Inputs count
  - Read Holding Registers count
  - Read Input Registers count
  - Write Single Coil count
  - Write Single Register count
  - Write Multiple Coils count
  - Write Multiple Registers count

Metrics are logged to a separate file every 30 seconds and can be output as JSON using the `--metrics-json` flag.

### Example Metrics Output

```json
{
  "timestamp": "2025-11-11T15:30:00Z",
  "connections": {
    "client": {
      "total": 15,
      "active": 3
    },
    "target": {
      "total": 8,
      "status": [
        {
          "address": "192.168.1.100:502",
          "connected": true
        }
      ]
    }
  },
  "requests": {
    "read_coils": 45,
    "read_discrete_inputs": 12,
    "read_holding_registers": 156,
    "read_input_registers": 89,
    "write_single_coil": 8,
    "write_single_register": 23,
    "write_multiple_coils": 3,
    "write_multiple_registers": 7
  }
}
```

## Logging

### Log Files

The proxy creates two types of log files with daily rotation:

1. **Regular Logs** (`modbus-proxy.{target}.log`):
   - General application logs
   - Connection events
   - Error messages

2. **Metrics Logs** (`modbus-proxy.{target}.metrics`):
   - Metrics snapshots (every 30 seconds)
   - Connection count updates
   - Request counter updates

### Log File Naming

Target addresses are sanitized for use in filenames:
- `192.168.1.100:502` → `modbus-proxy.192.168.1.100-502.log`

### Enable Detailed Logging

```bash
RUST_LOG=debug cargo run --release -- --target 192.168.1.100:502
```

Log levels:
- `error`: Error messages only
- `warn`: Warnings and errors
- `info`: General information (recommended)
- `debug`: Detailed debugging information
- `trace`: Very detailed tracing information

## Usage Examples

### Example 1: Basic Proxy Setup

```bash
# Start the proxy forwarding to a Modbus server
cargo run --release -- --target 192.168.1.100:502

# In another terminal, use modpoll to read coils
modpoll -m tcp -p 5020 127.0.0.1 1 10

# The request will be forwarded to 192.168.1.100:502 with Unit-ID 1
```

### Example 2: Monitoring with Metrics

```bash
# Get current metrics as JSON
cargo run --release -- --metrics-json

# Or watch metrics in real-time
tail -f logs/modbus-proxy.192.168.1.100-502.metrics
```

### Example 3: Production Deployment

```bash
# Build release binary
cargo build --release

# Run with custom settings
./target/release/modbus-proxy \
  --target 10.0.0.50:502 \
  --listen 0.0.0.0:5020 \
  --log-dir /var/log/modbus-proxy \
  --backend-timeout 30 \
  --strict true
```

## Timeout Handling

The proxy includes configurable timeout protection to prevent indefinite hanging when the target Modbus server is slow or unresponsive.

### How It Works

- **Backend Timeout**: Configurable timeout (default: 30 seconds) for all upstream operations
- **Connect Timeout**: Applies to initial connection establishment
- **Request Timeout**: Applies to each Modbus request/response cycle
- **Graceful Degradation**: Times out with Modbus exception instead of hanging

### Timeout Behavior

**In Strict Mode (default):**
- Timeout returns `GatewayPathUnavailable` (0x0A) exception
- Connection is dropped and will reconnect on next request

**In Non-Strict Mode:**
- Timeout returns `ServerDeviceFailure` (0x04) exception
- Connection is dropped and will reconnect on next request

### Configuration

```bash
# Set custom timeout (in seconds)
cargo run --release -- --target 192.168.1.100:502 --backend-timeout 10

# Very short timeout for testing
cargo run --release -- --target 192.168.1.100:502 --backend-timeout 1
```

### Testing Timeout Behavior

Use the included test suite to verify timeout functionality:

```bash
# Install Python dependencies
pip install -r requirements.txt

# Run simple timeout test
python3 test_simple_timeout.py

# Run interactive demo
./test_demo.sh
```

The test suite includes:
- `test_target_server.py`: Modbus server with configurable delays
- `test_simple_timeout.py`: Simple timeout demonstration
- `test_demo.sh`: Quick bash demo script
- `TESTING.md`: Comprehensive testing guide

### Timeout Log Messages

When a timeout occurs, you'll see:

```
WARN Connection to target 192.168.1.100:502 timed out after 10s
WARN Upstream request read_coils timed out after 10s
```

## Development

### Project Structure

```
modbus-proxy/
├── Cargo.toml          # Project dependencies
├── src/
│   └── main.rs         # Main proxy implementation
├── CODE_REVIEW.md      # Code review documentation
├── IMPLEMENTATION_SUMMARY.md  # Implementation details
├── MULTIPROCESS_LOGGING.md    # Multiprocess logging guide
├── TEST_COVERAGE.md    # Test coverage documentation
├── TESTING_SUMMARY.md  # Testing overview
├── QUEUE_USAGE.md      # Queue feature documentation (legacy)
├── CLIENT_USAGE.md     # Client/proxy documentation (legacy)
└── README.md          # This file
```

### Running Tests

```bash
# Run all tests
cargo test

# Run with output
cargo test -- --nocapture

# Run specific test
cargo test test_cli_default_values
```

### Code Quality

```bash
# Run clippy linter
cargo clippy

# Format code
cargo fmt

# Build for release
cargo build --release
```

The binary will be available at `target/release/modbus-proxy`

### Performance

- **Concurrent Connections**: Supports multiple simultaneous client connections
- **Async I/O**: Built on Tokio for high-performance async operations
- **Connection Pooling**: Reuses target server connections efficiently
- **Low Latency**: Direct request forwarding with minimal overhead

## Troubleshooting

### Connection Issues

1. **Cannot connect to target server**:
   - Verify target server address and port
   - Check network connectivity with `ping` or `telnet`
   - Ensure target server is running and accessible
   - Check firewall rules

2. **Port already in use**:
   - Change the listen port: `--listen 0.0.0.0:5021`
   - Or stop the other service using the port

3. **Client cannot connect to proxy**:
   - Verify proxy is listening: `netstat -tln | grep 5020`
   - Check firewall settings
   - Ensure correct listen address (use `0.0.0.0` for all interfaces)

### Build Issues

1. **Rust version too old**:
   ```bash
   rustup update
   ```

2. **Missing dependencies**:
   ```bash
   cargo clean
   cargo build
   ```

3. **Clippy warnings**:
   ```bash
   cargo clippy -- -D warnings
   ```

### Logging Issues

1. **No log files created**:
   - Ensure log directory exists and is writable
   - Check directory permissions
   - Verify `RUST_LOG` is set appropriately

2. **Too much log output**:
   - Set log level: `RUST_LOG=info` (instead of `debug`)
   - Use `--quiet` flag if available

## Contributing

1. Fork the repository
2. Create a feature branch (`git checkout -b feature/amazing-feature`)
3. Make your changes
4. Run tests and ensure they pass (`cargo test`)
5. Run clippy (`cargo clippy`)
6. Format code (`cargo fmt`)
7. Commit your changes (`git commit -m 'Add amazing feature'`)
8. Push to the branch (`git push origin feature/amazing-feature`)
9. Submit a pull request

## License

This project is licensed under the MIT License - see the LICENSE file for details.

## Related Documentation

- [Implementation Summary](IMPLEMENTATION_SUMMARY.md) - Architecture and design decisions
- [Code Review](CODE_REVIEW.md) - Code review notes and findings
- [Test Coverage](TEST_COVERAGE.md) - Detailed test coverage analysis
- [Testing Summary](TESTING_SUMMARY.md) - Testing approach and results
- [Multiprocess Logging](MULTIPROCESS_LOGGING.md) - Logging architecture
- [Queue Usage Guide](QUEUE_USAGE.md) - Queue feature documentation (legacy)
- [Client Usage Guide](CLIENT_USAGE.md) - Proxy client documentation (legacy)

## Acknowledgments

- Built with [tokio-modbus](https://github.com/slowtec/tokio-modbus)
- Uses [Tokio](https://tokio.rs/) for async runtime
- Logging with [tracing](https://github.com/tokio-rs/tracing)
- Command-line parsing with [clap](https://github.com/clap-rs/clap)
- Error handling with [anyhow](https://github.com/dtolnay/anyhow)
