#!/usr/bin/env python3
# Minimal forward HTTP proxy with optional per-chunk write delay, used by
# `tests/e2e/run-slow-net.sh` to inject latency between the node daemon and
# the control plane (Stage: slow-network failure injection).
#
# Listens on LISTEN (default 127.0.0.1:7820) and forwards every request to
# TARGET (default 127.0.0.1:7811), sleeping DELAY_MS (default 200) before
# each socket write. HTTP/1.1 only; no CONNECT (node→CP is plain HTTP).
#
# Props intentionally tiny: it just reads the full request, opens a socket to
# the target, relays bytes with a sleep on each send. Good enough to widen
# the round-trip so a chatty mock task sees slow-network conditions without
# touching the binary or `tc`.
import os, sys, socket, threading, time, select

LISTEN = os.environ.get("AG_PROXY_LISTEN", "127.0.0.1:7820")
TARGET = os.environ.get("AG_PROXY_TARGET", "127.0.0.1:7811")
DELAY_MS = float(os.environ.get("AG_PROXY_DELAY_MS", "200"))

def relay(src, dst):
    try:
        while True:
            r, _, _ = select.select([src], [], [], 30)
            if not r:
                continue
            data = src.recv(65536)
            if not data:
                break
            time.sleep(DELAY_MS / 1000.0)
            dst.sendall(data)
    except OSError:
        pass
    finally:
        try: dst.shutdown(socket.SHUT_WR)
        except OSError: pass

def handle(c, _):
    try:
        tgt = socket.create_connection(TARGET.split(":"))
    except OSError as e:
        c.sendall(b"HTTP/1.1 502 Bad Gateway\r\n\r\nproxy: " + str(e).encode())
        c.close(); return
    t1 = threading.Thread(target=relay, args=(c, tgt), daemon=True)
    t2 = threading.Thread(target=relay, args=(tgt, c), daemon=True)
    t1.start(); t2.start(); t1.join(); t2.join()
    c.close(); tgt.close()

def main():
    host, port = LISTEN.split(":"); port = int(port)
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind((host, port)); s.listen(64)
    print(f"throttle-proxy {LISTEN} -> {TARGET} delay={DELAY_MS}ms", flush=True)
    while True:
        c, _ = s.accept()
        threading.Thread(target=handle, args=(c, None), daemon=True).start()

if __name__ == "__main__":
    main()
