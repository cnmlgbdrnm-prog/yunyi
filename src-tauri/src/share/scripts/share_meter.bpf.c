/* Per-share public-interface IP accounting. Loopback is never counted.
 * Values are packet lengths at the cgroup IP hooks, not a provider invoice.
 */
#include <linux/bpf.h>

#define SEC(name) __attribute__((section(name), used))
#define __uint(name, value) int (*name)[value]
#define __type(name, value) value *name

static void *(*lookup)(void *, const void *) = (void *)BPF_FUNC_map_lookup_elem;
static long (*spin_lock)(struct bpf_spin_lock *) = (void *)BPF_FUNC_spin_lock;
static long (*spin_unlock)(struct bpf_spin_lock *) = (void *)BPF_FUNC_spin_unlock;

struct meter_config {
    struct bpf_spin_lock lock;
    __u32 pad;
    __u64 quota;
    __u64 history;
    __u32 ifindex;
    __u32 frozen;
};
struct meter_usage {
    struct bpf_spin_lock lock;
    __u32 pad;
    __u64 rx;
    __u64 tx;
    __u64 denied_quota;
};
struct {
    __uint(type, BPF_MAP_TYPE_ARRAY);
    __uint(max_entries, 1);
    __type(key, __u32);
    __type(value, struct meter_config);
} config SEC(".maps");
struct {
    __uint(type, BPF_MAP_TYPE_ARRAY);
    __uint(max_entries, 1);
    __type(key, __u32);
    __type(value, struct meter_usage);
} traffic SEC(".maps");

static __attribute__((always_inline)) int account(struct __sk_buff *skb, int ingress)
{
    __u32 key = 0, size = skb->len, dev = skb->ifindex;
    struct meter_config *cfg = lookup(&config, &key);
    struct meter_usage *usage = lookup(&traffic, &key);
    if (!cfg || !usage)
        return 0;
    spin_lock(&cfg->lock);
    __u64 quota = cfg->quota, history = cfg->history;
    __u32 iface = cfg->ifindex, frozen = cfg->frozen;
    spin_unlock(&cfg->lock);
    if (dev == 1)
        return 1;
    /* Do not silently let traffic escape accounting through another route. */
    if (dev != iface || frozen)
        return 0;
    int allow = 1;
    spin_lock(&usage->lock);
    __u64 used = usage->rx + usage->tx;
    if (ingress) {
        /* These bytes have arrived even if the quota rejects delivery. Never
         * clamp the real ingress count to the quota or silently discard it. */
        usage->rx += size;
        used += size;
        if (quota && (history >= quota || used >= quota - history)) {
            allow = 0;
            usage->denied_quota = quota;
        }
    } else if (quota && (history >= quota || used >= quota - history
                   || size > quota - history - used)) {
        allow = 0;
        usage->denied_quota = quota;
    } else {
        usage->tx += size;
    }
    spin_unlock(&usage->lock);
    return allow;
}

SEC("cgroup_skb/ingress")
int share_ingress(struct __sk_buff *skb) { return account(skb, 1); }
SEC("cgroup_skb/egress")
int share_egress(struct __sk_buff *skb) { return account(skb, 0); }
char LICENSE[] SEC("license") = "GPL";
