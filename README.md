# Wakezilla 🦖
![Crates.io Version](https://img.shields.io/crates/v/wakezilla) [![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT) [![CI](https://github.com/guibeira/wakezilla/actions/workflows/ci.yml/badge.svg)](https://github.com/guibeira/wakezilla/actions/workflows/ci.yml)
<img width="200" height="159" src="https://github.com/user-attachments/assets/e88f084b-47b8-467b-a5c6-d64327805792" align="left" alt="wakezilla"/>

⚡ Wake-on-LAN made simple → power on your machines remotely whenever needed.

🌐 Reverse proxy → intercepts traffic and wakes the server automatically if it’s offline.

🔌 Automatic shutdown → saves energy by powering down idle machines after configurable thresholds.



## Web interface
<img width="531" height="727" alt="image" src="https://github.com/user-attachments/assets/e9e744c4-35ec-4ca0-8de2-696e447cce7a" />

## Features

- **Wake-on-LAN**: Send magic packets to wake sleeping machines
- **TCP Proxy**: Forward ports to remote machines with automatic WOL
- **Web Interface**: Manage machines, ports, and monitor activity through a web dashboard
- **Automatic Shutdown**: Automatically turn off machines after inactivity periods
- **Network Scanner**: Discover machines on your local network

## Installation

### Install from cargo (recommended)

```bash
cargo install wakezilla
```

### Install via Homebrew

```bash
brew tap guibeira/wakezilla
brew install wakezilla
```

### Using pre-built docker image

> Docker images are published automatically on every `v*` release tag.
> Supported platforms: `linux/amd64`, `linux/arm64`.
> Stable releases move `guibeira/wakezilla:latest`; prerelease tags publish versioned tags only.
> Prefer `guibeira/wakezilla:<version>` for pinned server deployments.

1. **Run the proxy server**:
```bash
docker run -d \
 --name wakezilla-proxy \
 --network host \
 -e WAKEZILLA__SERVER__PROXY_PORT=3000 \
 -v ${PWD}/wakezilla-data:/opt/wakezilla \
 guibeira/wakezilla:latest proxy-server
```
Note:
- `--network host` is required for Wake-on-LAN to work properly.
- add `-v ${PWD}/wakezilla-data:/opt/wakezilla` to save configuration data persistently.

2. **Run the client server**:
```bash
docker run -d \
 --name wakezilla-client \
 -p 3001:3001 \
 guibeira/wakezilla:latest client-server
```

### Install from source

1. **Install Rust**:
   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   source $HOME/.cargo/env
   ```

2. **Build and Install**:
   ```bash
   git clone git@github.com:guibeira/wakezilla.git
   cd wakezilla
   make install
   ```

3. **Verify Installation**:
   ```bash
   wakezilla --version
   ```
### Run proxy server 

1. **Run the Server**:
   ```bash
    wakezilla proxy-server
   ```
   
   By default, the web interface runs on port 3000.

### Run Client 

1. **Run the Server**:
   ```bash
    wakezilla client-server
   ```
   
   By default, the web interface runs on port 3001.
   You can check the health of the client server by visiting:
   http://<client-ip>:3001/health


## Usage

### Web Interface
Access the web interface at `http://<server-ip>:3000` to:
- Add and manage machines
- Configure port forwards
- View network scan results
- Send WOL packets manually
- Configure automatic shutdown settings

### Adding Machines
1. Navigate to the web interface
2. Click "Add Machine" or use the network scanner
3. Fill in MAC address, IP, and name
4. Configure:
   - Turn-off port (if remote shutdown is needed)
   - Inactivity Period: Time in minutes before automatic shutdown (default: 30 minutes)
   - Port forwards as needed

### Configuring Automatic Shutdown
1. When adding or editing a machine, enable "Can be turned off remotely"
2. Set the "Turn Off Port" (typically 3001 for the client server)
3. Configure the Inactivity Period:
   - Set the number of minutes of inactivity before automatic shutdown
   - The system monitors when the last request was received for each machine
   - If no requests are received within the inactivity period, the machine will be automatically shut down
4. The machine will automatically shut down after the configured inactivity period of no activity

### Port Forwarding
1. Add a machine to the system
2. Configure port forwards for that machine:
   - Local Port: Port on the server to listen on
   - Target Port: Port on the remote machine to forward to
3. When traffic hits the local port, the machine will be woken up if needed and traffic forwarded


### Machine Configuration
Each machine can be configured with:
- MAC Address
- IP Address
- Name and Description
- Turn-off Port (for remote shutdown)
- Inactivity Period: Time in minutes before automatic shutdown (default: 30 minutes)
- Port Forwards:
  - Local Port: Port on the server
  - Target Port: Port on the remote machine

## How It Works

1. **Server Mode**: Runs the web interface and proxy services
2. **Client Mode**: Runs on target machines to enable remote shutdown
3. **WOL Process**: 
   - When traffic hits a configured port, the server sends a WOL packet
   - Waits for the machine to become reachable
   - Forwards traffic once the machine is up
4. **Automatic Shutdown**: 
   - A **single global inactivity monitor** runs continuously, checking all machines every second
   - Each machine's `last_request` timestamp is automatically updated whenever a connection is accepted
   - The monitor compares the time since `last_request` against the configured `inactivity_period` (in minutes)
   - If no requests are received within the inactivity period, a shutdown signal is sent via HTTP to the client
   - When a machine configuration is updated (e.g., inactivity period changed), the monitor is automatically stopped and restarted with the new settings
   - This ensures only one monitor instance runs at a time, preventing duplicate shutdown signals

## Security Considerations

- The server should be run on a trusted network
- Access to the web interface should be restricted if exposed to the internet
- The turn-off endpoint on clients should only be accessible from the server

## Development
### Prerequisites
- Rust and Cargo installed
- Clone the repository
- Install dependencies with `make dependencies`

on frontend folder run:
```bash
trunk serve
```
this will initialize the frontend in watch mode on port 8080

on the root folder run:
```bash
cargo watch -x 'run -- proxy-server'
```
this will initialize the backend in watch mode on port 3000


## Troubleshooting

### Common Issues

1. **Machine not waking up**:
   - Verify the MAC address is correct
   - Ensure WOL is enabled in the machine's BIOS/UEFI
   - Check firewall settings on the target machine
   - Verify the target machine supports WOL

2. **Proxy not working**:
   - Check that the target port is correct
   - Verify the machine is reachable after WOL
   - Ensure no firewall is blocking the connection

3. **Automatic shutdown not working**:
   - Verify the turn-off port is configured correctly
   - Ensure the client is running on the target machine
   - Check that the client can receive HTTP requests from the server
   - Verify the inactivity period is configured correctly (in minutes)
   - Check logs to see when the last request was received for the machine
   - Ensure traffic is actually reaching the proxy (requests update the last_request timestamp)

### Logs
Check the terminal output for detailed logs about:
- WOL packets sent
- Connection attempts
- Proxy activity
- Shutdown requests
- Errors and warnings

## Contributing

1. Fork the repository
2. Create a feature branch
3. Make your changes
4. Run `cargo fmt` and `cargo clippy`
5. Commit your changes
6. Push to the branch
7. Create a pull request

## License

This project is licensed under the MIT License - see the LICENSE file for details.
