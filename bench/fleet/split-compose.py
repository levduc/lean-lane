#!/usr/bin/env python3
"""split-compose.py — turn quake's one-machine compose into a multi-machine fleet.

Called by `bench/fleet/fleet up` right after `quake setup` rendered the testnet.

Reads  <quake-dir>/compose.yaml, writes
  <quake-dir>/compose.yaml              validator1 only, peers at the fleet IPs
  <quake-dir>/compose-val{2..N}.yaml    one validator's CL+EL, remote paths
  <quake-dir>/compose.yaml.prefleet     the untouched original

Substitutions (each one is printed by --dry-run):
  CL  --p2p.persistent-peers  /ip4/172.21.1.{n-1}/tcp/27000  ->  /ip4/<ip_n>/tcp/{27000+n-1}
      (quake already publishes {27000+n-1}:27000 for validator n, so the port is
       reachable on the host as-is; we only retarget the dial address)
  EL  --trusted-peers enode://..@172.21.2.{n-1}:30303  ->  ..@<ip_n>:{30300+n}
      and we ADD the publish {30300+n}:30303, which quake does not render.
  volumes  ./validatorN/..  ->  <fleet-root>/<run-id>/quake/validatorN/..  (remote files)
           so the compose file can live anywhere on the remote.

The remote `default` network keeps `internal: true` (containers reach the outside
only through `host-access`) but DROPS the 172.21.0.0/16 ipam block and the static
ipv4_address of each service: one validator per host does not need fixed addresses,
and a hardcoded subnet can collide with whatever else that machine runs.
"""
import argparse, copy, os, sys, yaml

CL_IP = lambda n: f"172.21.1.{n - 1}"
EL_IP = lambda n: f"172.21.2.{n - 1}"
CL_HOSTPORT = lambda n: 27000 + (n - 1)
EL_HOSTPORT = lambda n: 30300 + n


def subs_for(ips):
    """[(old, new, what)] for every validator index in ips."""
    out = []
    for n in sorted(ips):
        out.append((f"/ip4/{CL_IP(n)}/tcp/27000", f"/ip4/{ips[n]}/tcp/{CL_HOSTPORT(n)}", f"val{n} CL peer"))
        out.append((f"@{EL_IP(n)}:30303", f"@{ips[n]}:{EL_HOSTPORT(n)}", f"val{n} EL enode"))
    return out


def rewrite(arg, subs):
    for old, new, _ in subs:
        arg = arg.replace(old, new)
    return arg


def strip_local_paths(volumes, remote_quake):
    """./validatorN/x  and  ./assets, ./logs  ->  <remote_quake>/..."""
    out = []
    for v in volumes:
        if v.startswith("./"):
            v = f"{remote_quake}/{v[2:]}"
        out.append(v)
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--quake-dir", required=True,
                    help="the rendered testnet, <arc checkout>/.quake/<scenario>")
    ap.add_argument("--run-id", default="RUN")
    ap.add_argument("--fleet-root", required=True,
                    help="run root on every machine (the same absolute path everywhere)")
    ap.add_argument("--ips", required=True,
                    help="comma-separated IPs for validators 1..N, in order")
    ap.add_argument("--dry-run", action="store_true")
    a = ap.parse_args()

    ips = {i + 1: ip for i, ip in enumerate(a.ips.split(",")) if ip}
    base = a.quake_dir
    compose = os.path.join(base, "compose.yaml")
    remote_quake = f"{a.fleet_root}/{a.run_id}/quake"
    subs = subs_for(ips)

    print(f"split-compose: {base} run-id={a.run_id}")
    print(f"  compose     {compose}")
    print(f"  remote root {remote_quake}")
    for old, new, what in subs:
        print(f"  {what:16s} {old}  ->  {new}")
    for n in sorted(ips):
        if n == 1:
            print(f"  val1 EL publish  {EL_HOSTPORT(n)}:30303 (local)")
        else:
            print(f"  val{n} EL publish  {EL_HOSTPORT(n)}:30303  CL publish {CL_HOSTPORT(n)}:27000 -> compose-val{n}.yaml")
    if a.dry_run:
        print("dry run: nothing written")
        return 0

    if not os.path.exists(compose):
        print(f"FAIL: no {compose} — run quake setup on the scenario first", file=sys.stderr)
        return 1
    c = yaml.safe_load(open(compose))
    orig = copy.deepcopy(c)

    # keep only the validator services this fleet uses (drop blockscout etc.)
    keep = {f"validator{n}_{k}" for n in ips for k in ("cl", "el")}
    for name in [s for s in c["services"] if s not in keep]:
        del c["services"][name]
    for netname in [n for n in c.get("networks", {}) if n not in ("default", "host-access")]:
        del c["networks"][netname]
    c.pop("secrets", None)

    # retarget every peer/enode reference, and publish the EL p2p port everywhere
    for n in ips:
        cl = c["services"][f"validator{n}_cl"]
        cl["command"] = [rewrite(x, subs) for x in cl["command"]]
        el = c["services"][f"validator{n}_el"]
        el["command"] = [rewrite(x, subs) for x in el["command"]]
        pub = f"{EL_HOSTPORT(n)}:30303"
        ports = el.setdefault("ports", [])
        if pub not in ports:
            ports.append(pub)

    # split validators 2..N out to their own hosts
    for n in sorted(ips):
        if n == 1:
            continue
        svcs = {}
        for kind in ("cl", "el"):
            name = f"validator{n}_{kind}"
            svc = c["services"].pop(name)
            svc["volumes"] = strip_local_paths(svc["volumes"], remote_quake)
            nets = svc.get("networks")
            if isinstance(nets, dict):
                for v in nets.values():
                    if isinstance(v, dict):
                        v.pop("ipv4_address", None)
                svc["networks"] = {k: (None if not v else v) for k, v in nets.items()}
            svcs[name] = svc
        remote = {
            "name": "arc_testnet",
            "services": svcs,
            "networks": {
                "default": {"name": "arc_testnet_default", "driver": "bridge", "internal": True},
                "host-access": {"name": "arc_testnet_host-access", "driver": "bridge"},
            },
        }
        out = os.path.join(base, f"compose-val{n}.yaml")
        open(out, "w").write(yaml.dump(remote, sort_keys=False))
        print(f"  wrote {out}")

    open(compose + ".prefleet", "w").write(yaml.dump(orig, sort_keys=False))
    open(compose, "w").write(yaml.dump(c, sort_keys=False))
    print(f"  wrote {compose} (validator1 only; original kept as compose.yaml.prefleet)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
