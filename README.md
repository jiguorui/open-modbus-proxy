# Modbus TCP Proxy

A high-performance, asynchronous Modbus TCP proxy server built with Rust and Tokio. This proxy forwards Modbus TCP requests from clients to a target Modbus server with Unit-ID passthrough, providing robust connection management and comprehensive metrics.

## Features

- **Unit-ID Passthrough**: Forwards client Unit-IDs transparently to the target server
- **Connection Management**: Automatic reconnection to target server with connection state tracking
- **Configurable Timeouts**: Backend timeout protection prevents indefinite hanging
- **Comprehensive Metrics**: Built-in metrics tracking with JSON snapshot output
- **Structured Logging**: Separate log files for regular logs and metrics with daily rotation
- **Strict Mode**: Configurable error mapping (GatewayPathUnavailable vs ServerDeviceFailure)
- **High Performance**: Built on Tokio for async I/O handling
- **Concurrent Connections**: Supports multiple simultaneous client connections

## Quick Start

### Prerequisites

- Rust 1.70+
- Cargo

### Build & Run

```bash
# Clone and build
git clone <repository-url>
cd modbus-proxy
cargo build --release

# Run proxy
cargo run --release -- --target 192.168.1.100:502

# With custom options
cargo run --release -- --target 192.168.1.100:502 --listen 127.0.0.1:8080 --log-dir ./logs
```

### Docker Usage

```bash
docker build -t modbus-proxy .
docker run -p 5020:5020 modbus-proxy --target 192.168.1.100:502
```

## Architecture

```
┌─────────────────┐
│ Modbus Clients  │
└────────┬────────┘
         │
         │ Connects to
         │
┌────────▼────────┐
│ Modbus TCP Proxy│
│ (Port 5020)     │
└────────┬────────┘
         │
         │ Forwards requests with
         │ Unit-ID passthrough
         │
┌────────▼────────┐
│ Target Modbus   │
│ Server          │
│ (Port 502)      │
└─────────────────┘
```

## Configuration

### Command-Line Options

| Option | Default | Description |
|--------|---------|-------------|
| `-t, --target <ADDR>` | *Required* | Target Modbus server address |
| `-l, --listen <ADDR>` | `0.0.0.0:5020` | Listen address for proxy |
| `--strict <BOOL>` | `true` | Use GatewayPathUnavailable for errors |
| `--log-dir <DIR>` | `./logs` | Log directory for file output |
| `--backend-timeout <SECS>` | `30` | Backend timeout in seconds |
| `--metrics-json` | - | Output metrics as JSON and exit |
| `--no-console` | `false` | Disable console logging |
| `-h, --help` | - | Print help information |

### Environment Variables

- `RUST_LOG`: Logging level (`error`, `warn`, `info`, `debug`, `trace`)

### Common Usage Examples

```bash
# Basic usage
cargo run --release -- --target 192.168.1.100:502

# Custom listen port
cargo run --release -- --target 192.168.1.100:502 --listen 0.0.0.0:5021

# Non-strict mode (ServerDeviceFailure errors)
cargo run --release -- --target 192.168.1.100:502 --strict false

# Disable console output
cargo run --release -- --target 192.168.1.100:502 --no-console

# Output metrics JSON
cargo run --release -- --metrics-json

# Debug logging
RUST_LOG=debug cargo run --release -- --target 192.168.1.100:502
```

## Supported Function Codes

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

Metrics are logged every 30 seconds to a separate metrics file and can be viewed via JSON output.

### Metrics Structure

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
      "connected": true
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

Two types of log files with daily rotation:

1. **Regular Logs** (`modbus-proxy.{target}.log`): General application logs
2. **Metrics Logs** (`modbus-proxy.{target}.metrics`): Metrics snapshots

Target addresses are sanitized for filenames:
- `192.168.1.100:502` → `modbus-proxy.192.168.1.100-502.log`

## Timeout Handling

Configurable timeout protection prevents hanging on slow/unresponsive targets.

### Timeout Behavior

**Strict Mode (default):**
- Returns `GatewayPathUnavailable` (0x0A) exception on timeout
- Connection is dropped and reconnects on next request

**Non-Strict Mode:**
- Returns `ServerDeviceFailure` (0x04) exception on timeout
- Connection is dropped and reconnects on next request

```bash
# Set custom timeout (1-60 seconds)
cargo run --release -- --target 192.168.1.100:502 --backend-timeout 10
```

## Development

### Build & Test

```bash
# Run tests
cargo test

# Run with output
cargo test -- --nocapture

# Code quality
cargo clippy
cargo fmt

# Build release
cargo build --release
```

Binary will be at `target/release/modbus-proxy`

### Performance Features

- **Concurrent Connections**: Multiple simultaneous client connections
- **Async I/O**: Built on Tokio for high-performance operations
- **Connection Pooling**: Efficient target server connection reuse
- **Low Latency**: Direct request forwarding with minimal overhead

## Troubleshooting

### Connection Issues

1. **Cannot connect to target**:
   - Verify target address and port
   - Check network connectivity with `ping` or `telnet`
   - Ensure target server is running and accessible
   - Check firewall rules

2. **Port already in use**:
   - Change listen port: `--listen 0.0.0.0:5021`
   - Or stop the service using the port

3. **Client cannot connect**:
   - Verify proxy is listening: `netstat -tln | grep 5020`
   - Check firewall settings

### Build Issues

```bash
# Update Rust
rustup update

# Clean and rebuild
cargo clean && cargo build
```

### Logging Issues

- Ensure log directory exists and is writable
- Check `RUST_LOG` environment variable
- Reduce log level: `RUST_LOG=info`

## Contributing

1. Fork the repository
2. Create feature branch: `git checkout -b feature/amazing-feature`
3. Make changes
4. Run tests: `cargo test`
5. Run clippy: `cargo clippy`
6. Format code: `cargo fmt`
7. Commit: `git commit -m 'Add amazing feature'`
8. Push: `git push origin feature/amazing-feature`
9. Submit pull request

## License

This project is licensed under the MIT License - see the LICENSE file for details.

## Related Documentation

- [Implementation Summary](IMPLEMENTATION_SUMMARY.md) - Architecture and design decisions
- [Test Coverage](TEST_COVERAGE.md) - Detailed test coverage analysis
- [Code Review](CODE_REVIEW.md) - Code review notes

## Acknowledgments

- Built with [tokio-modbus](https://github.com/slowtec/tokio-modbus)
- Uses [Tokio](https://tokio.rs/) for async runtime
- Logging with [tracing](https://github.com/tokio-rs/tracing)
- Command-line parsing with [clap](https://github.com/clap-rs/clap)
- Error handling with [anyhow](https://github.com/dtolnay/anyhow)