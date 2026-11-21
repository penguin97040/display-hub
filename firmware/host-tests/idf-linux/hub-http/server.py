# A stand-in hub for the hub_http.c harness (../README.md): one behaviour per port, on 127.0.0.1
# only. Python 3, standard library only.
import os, socket, ssl, sys, threading, time
C = sys.argv[1]  # the folder of test certificates run.sh makes
BASE = 19000

def ctx(cert, maxver=None):
    c = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    c.load_cert_chain(os.path.join(C, cert + ".pem"), os.path.join(C, cert + ".key"))
    if maxver: c.maximum_version = maxver
    return c

class Conn:
    """TLS over memory BIOs, so the raw bytes can be cut up and delayed."""
    def __init__(self, sock, context):
        self.s = sock; self.inb = ssl.MemoryBIO(); self.outb = ssl.MemoryBIO()
        self.o = context.wrap_bio(self.inb, self.outb, server_side=True)
    def flush(self, split=None, gap=0.0):
        data = self.outb.read()
        if not data: return
        if split:
            for i in range(0, len(data), split):
                self.s.sendall(data[i:i + split]); time.sleep(gap)
        else:
            self.s.sendall(data)
    def pump(self, fn, *a):
        while True:
            try:
                r = fn(*a); self.flush(); return r
            except ssl.SSLWantReadError:
                self.flush()
                d = self.s.recv(65536)
                if not d: raise ConnectionError("closed")
                self.inb.write(d)
    def handshake(self): self.pump(self.o.do_handshake)
    def request(self):
        buf = b""
        while b"\r\n\r\n" not in buf:
            buf += self.pump(self.o.read, 65536)
        head, _, body = buf.partition(b"\r\n\r\n")
        n = 0
        for line in head.split(b"\r\n"):
            if line.lower().startswith(b"content-length:"): n = int(line.split(b":")[1])
        while len(body) < n: body += self.pump(self.o.read, 65536)
        return head, body
    def send(self, data, split=None, gap=0.0):
        # The hub's records are at most 4 KiB: the display's receive buffer is that size.
        for i in range(0, len(data), 3000):
            self.o.write(data[i:i + 3000]); self.flush(split, gap)

def answer(body, status=b"200 OK", extra=b""):
    return b"HTTP/1.1 " + status + b"\r\nContent-Type: application/json\r\nContent-Length: " + str(len(body)).encode() + b"\r\nConnection: close\r\n" + extra + b"\r\n" + body

def honest(c):
    c.handshake(); head, body = c.request()
    first = head.split(b"\r\n")[0]
    c.send(answer(b'{"first":"' + first + b'","body_len":' + str(len(body)).encode() + b',"has_auth":' + (b"true" if b"authorization: bearer" in head.lower() else b"false") + b'}'))
def split_records(c):
    c.handshake(); c.request()
    # Every 7 bytes of the encrypted answer in a TCP segment of its own, 30 ms apart.
    c.send(answer(b'{"split":true,"pad":"' + b"p" * 200 + b'"}'), split=7, gap=0.03)
def handshake_trickle(c):
    c.s.recv(65536)
    c.s.sendall(b"\x16\x03\x03\x0f\xa0")  # a handshake record of 4000 bytes, it says
    while True:
        c.s.sendall(b"\x02"); time.sleep(0.5)
def silent(c):
    time.sleep(3600)
def headers_trickle(c):
    c.handshake(); c.request(); c.send(b"HTTP/1.1 200 OK\r\n")
    while True:
        for ch in b"X-Slow: abcdefghijklmnopqrstuvwxyz\r\n":
            c.send(bytes([ch])); time.sleep(0.5)
def headers_flood(c):
    c.handshake(); c.request(); c.send(b"HTTP/1.1 200 OK\r\n")
    while True: c.send(b"X-Pad: yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy\r\n" * 20)
def garbage_flood(c):
    c.handshake(); c.request()
    while True: c.send(os.urandom(1024))
def body_trickle(c):
    c.handshake(); c.request(); c.send(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n")
    while True: c.send(b"x"); time.sleep(0.5)
def body_quiet(c):
    c.handshake(); c.request(); c.send(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\nabc"); time.sleep(3600)
def download(c):
    c.handshake(); c.request(); blob = bytes(range(256)) * 400
    c.send(b"HTTP/1.1 200 OK\r\nContent-Length: " + str(len(blob)).encode() + b"\r\n\r\n")
    for i in range(0, len(blob), 3000): c.send(blob[i:i + 3000])
def redirect(c):
    c.handshake(); c.request(); c.send(answer(b'{"error":"moved"}', b"302 Found", b"Location: https://example.invalid/\r\n"))
def too_long(c):
    c.handshake(); c.request(); c.send(answer(b"z" * 5000))
def no_request_read(c):
    # Finishes the handshake, then never reads: for the request's own write.
    c.handshake(); time.sleep(3600)

PORTS = [
    (1, "hub", None, honest), (2, "other", None, honest), (3, "wrongname", None, honest),
    (4, "hub", None, handshake_trickle), (5, "hub", None, silent), (6, "hub", None, headers_trickle),
    (7, "hub", None, headers_flood), (8, "hub", None, garbage_flood), (9, "hub", None, split_records),
    (10, "hub", None, body_trickle), (11, "hub", None, download), (12, "hub", None, redirect),
    (13, "hub", ssl.TLSVersion.TLSv1_2, honest), (14, "hub", None, too_long), (15, "hub", None, body_quiet),
    (16, "hub", ssl.TLSVersion.TLSv1_2, split_records), (17, "hub", None, no_request_read),
]

def serve(port, cert, maxver, fn):
    context = ctx(cert, maxver)
    l = socket.socket(); l.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    l.bind(("127.0.0.1", BASE + port)); l.listen(16)
    def one(s):
        s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        try: fn(Conn(s, context))
        except Exception: pass
        finally:
            try: s.close()
            except Exception: pass
    while True:
        s, _ = l.accept()
        threading.Thread(target=one, args=(s,), daemon=True).start()

for p in PORTS: threading.Thread(target=serve, args=p, daemon=True).start()
print("ready", flush=True)
while True: time.sleep(3600)
