#!/bin/bash
# Setup script for agent host machine
# Run once as root before starting agents
# Assumes Ubuntu Linux x64

set -euo pipefail

echo "=== Nova Agent Host Setup ==="

# Check if running as root
if [ "$EUID" -ne 0 ]; then
    echo "Please run as root"
    exit 1
fi

# 1. Disable swap for agent processes (critical!)
echo "Disabling swap..."
swapoff -a || true
# Comment out swap in fstab
sed -i '/swap/s/^/#/' /etc/fstab

# 2. Set up cgroups v2
echo "Setting up cgroups..."
if ! mount | grep -q "cgroup2"; then
    echo "cgroup2 not mounted, attempting to mount..."
    mount -t cgroup2 none /sys/fs/cgroup 2>/dev/null || true
fi

# Create agent parent cgroup
CGROUP_ROOT="/sys/fs/cgroup/nova-agents"
mkdir -p "$CGROUP_ROOT"

# Enable memory and pids controllers
echo "+memory +pids" > "$CGROUP_ROOT/cgroup.subtree_control" 2>/dev/null || {
    # If that fails, we might need to enable at root level first
    echo "+memory +pids" > /sys/fs/cgroup/cgroup.subtree_control 2>/dev/null || true
    echo "+memory +pids" > "$CGROUP_ROOT/cgroup.subtree_control" 2>/dev/null || true
}

# 3. Create workspace directories
echo "Creating workspace directories..."
WORKSPACE_ROOT="/var/nova-agents"
mkdir -p "$WORKSPACE_ROOT"/{pool,active,scratch}
chmod 755 "$WORKSPACE_ROOT"

# 4. Pre-create workspace pool (500 slots)
echo "Creating workspace pool..."
for i in $(seq -w 1 500); do
    ws="$WORKSPACE_ROOT/pool/ws-$i"
    mkdir -p "$ws"
done

# 5. Set up tmpfs for scratch (optional, helps with temp file speed)
echo "Setting up scratch tmpfs..."
if ! mount | grep -q "$WORKSPACE_ROOT/scratch"; then
    mount -t tmpfs -o size=50G tmpfs "$WORKSPACE_ROOT/scratch"
    # Add to fstab for persistence
    if ! grep -q "$WORKSPACE_ROOT/scratch" /etc/fstab; then
        echo "tmpfs $WORKSPACE_ROOT/scratch tmpfs size=50G 0 0" >> /etc/fstab
    fi
fi

# 6. Install monitoring tools
echo "Installing monitoring tools..."
apt-get update -qq
apt-get install -y -qq htop iotop sysstat

# 7. Set kernel parameters for many processes
echo "Tuning kernel parameters..."
cat >> /etc/sysctl.d/99-nova-agents.conf << 'EOF'
# Nova agent tuning
vm.overcommit_memory = 0
vm.overcommit_ratio = 80
kernel.pid_max = 4194304
fs.file-max = 2097152
fs.inotify.max_user_watches = 524288
fs.inotify.max_user_instances = 1024
EOF
sysctl -p /etc/sysctl.d/99-nova-agents.conf

# 8. Set system-wide limits
echo "Setting system limits..."
cat >> /etc/security/limits.d/99-nova-agents.conf << 'EOF'
# Nova agent limits
*    soft    nofile    65536
*    hard    nofile    131072
*    soft    nproc     65536
*    hard    nproc     131072
EOF

# 9. Copy scripts to /opt/nova/scripts
echo "Installing scripts..."
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
mkdir -p /opt/nova/scripts
cp "$SCRIPT_DIR"/*.sh /opt/nova/scripts/ 2>/dev/null || true
chmod +x /opt/nova/scripts/*.sh 2>/dev/null || true

echo ""
echo "=== Setup complete ==="
echo ""
echo "Workspace pool: $WORKSPACE_ROOT/pool (500 slots)"
echo "Active workspaces: $WORKSPACE_ROOT/active"
echo "Scratch space: $WORKSPACE_ROOT/scratch (50GB tmpfs)"
echo "Cgroup root: $CGROUP_ROOT"
echo "Scripts installed: /opt/nova/scripts/"
echo ""
echo "Next: Use spawn-agent.sh to start agents"
