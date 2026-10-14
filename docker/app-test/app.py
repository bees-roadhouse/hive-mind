"""The container tier's app: hive-apps' protocol over a unix socket, nothing
else. /ready answers 200. /invoke echoes `data` back trusted and adds what
the test wants to know about the sandbox from inside: whether the
credential arrived by environment (never its value: it is a secret and the
test resolves the one the launcher recorded instead), whether the entry
variable arrived, and whether a TCP connect to the outside succeeds, which
under --network=none it must not. A `sleep` field makes the handler hang,
for the deadline test."""

import json
import os
import socket
import time
from http.server import BaseHTTPRequestHandler, HTTPServer
from socketserver import UnixStreamServer

SOCK = os.environ.get("HIVE_APP_SOCKET", "/run/app/app.sock")


def network_reachable():
    try:
        s = socket.create_connection(("1.1.1.1", 53), timeout=1.5)
        s.close()
        return True
    except OSError:
        return False


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def _send(self, code, body):
        raw = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self):
        if self.path == "/ready":
            return self._send(200, {"ready": True})
        self._send(404, {"error": "no such path"})

    def do_POST(self):
        if self.path != "/invoke":
            return self._send(404, {"error": "no such path"})
        n = int(self.headers.get("Content-Length") or 0)
        req = json.loads(self.rfile.read(n) or b"{}")
        data = req.get("data")
        if isinstance(data, dict) and "sleep" in data:
            time.sleep(float(data["sleep"]))
        self._send(
            200,
            {
                "trust": "trusted",
                "data": {
                    "echo": data,
                    "kind": req.get("kind"),
                    "name": req.get("name"),
                    "caller": req.get("caller"),
                    "has_credential": bool(os.environ.get("HIVE_APP_CREDENTIAL")),
                    "entry": os.environ.get("HIVE_APP_ENTRY"),
                    "install": os.environ.get("HIVE_APP_INSTALL"),
                    "network": network_reachable(),
                    "cwd": os.getcwd(),
                    "uid": os.getuid(),
                },
            },
        )


class Server(UnixStreamServer, HTTPServer):
    # HTTPServer's server_bind wants a TCP name; a unix server has a path.
    def server_bind(self):
        UnixStreamServer.server_bind(self)
        self.server_name = "app"
        self.server_port = 0


if __name__ == "__main__":
    try:
        os.unlink(SOCK)
    except FileNotFoundError:
        pass
    srv = Server(SOCK, Handler)
    os.chmod(SOCK, 0o660)
    srv.serve_forever()
