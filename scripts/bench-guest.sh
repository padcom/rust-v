# Benchmarks run inside the Ubuntu server guest (see scripts/ubuntu-bench.py).
# Each prints "bench <name> <seconds>".
# Best of three runs.
b() {
  local name=$1; shift
  local best=
  for _ in 1 2 3; do
    local t0=$(date +%s%N); "$@" >/dev/null 2>&1; local t1=$(date +%s%N)
    local ms=$(( (t1 - t0) / 1000000 ))
    if [ -z "$best" ] || [ $ms -lt $best ]; then best=$ms; fi
  done
  printf 'bench %s %d.%03d\n' "$name" $((best / 1000)) $((best % 1000))
}
# Let first-boot services (snapd seeding, cloud-init) finish first.
systemctl is-system-running --wait >/dev/null 2>&1
head -c 64M /dev/urandom > /tmp/rand.bin
seq 1 3000000 > /tmp/lines.txt
b python  python3 -c 'd={}
for i in range(4500000): d[i%1000]=d.get(i%1000,0)+i*i
print(sum(d.values()))'
b xz      sh -c 'head -c 16M /tmp/rand.bin | xz -1 -T1 > /dev/null'
b gzip    sh -c 'gzip -1 -c /tmp/lines.txt > /dev/null'
b sha256  sha256sum /tmp/rand.bin
b grep    grep -c 99999 /tmp/lines.txt
b sort    sort -r /tmp/lines.txt -o /tmp/sorted.txt
b memcpy  dd if=/tmp/rand.bin of=/dev/null bs=1M
