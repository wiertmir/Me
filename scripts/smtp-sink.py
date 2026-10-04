#!/usr/bin/env python3
"""Minimal SMTP sink for tests: accepts any mail and writes each message's raw DATA to DIR/NNNN.eml.

Usage: smtp-sink.py DIR [PORT]   (listens on 127.0.0.1, default port 2525). No auth, no TLS: tests only.
"""
import itertools
import pathlib
import socketserver
import sys
import threading

out = pathlib.Path(sys.argv[1])
port = int(sys.argv[2]) if len(sys.argv) > 2 else 2525
out.mkdir(parents=True, exist_ok=True)
counter = itertools.count(1)
lock = threading.Lock()


class Handler(socketserver.StreamRequestHandler):
    def reply(self, text):
        self.wfile.write(text.encode() + b"\r\n")
        self.wfile.flush()

    def handle(self):
        self.reply("220 smtp-sink ready")
        for raw in self.rfile:
            cmd = raw.decode(errors="replace").strip().upper()
            if cmd.startswith(("EHLO", "HELO")):
                self.reply("250 smtp-sink")
            elif cmd.startswith("DATA"):
                self.reply("354 end with <CRLF>.<CRLF>")
                lines = []
                for line in self.rfile:
                    if line == b".\r\n":
                        break
                    lines.append(line[1:] if line.startswith(b"..") else line)
                with lock:
                    n = next(counter)
                tmp = out / f"{n:04d}.tmp"
                tmp.write_bytes(b"".join(lines))
                tmp.rename(out / f"{n:04d}.eml")  # appears whole or not at all
                self.reply("250 queued")
            elif cmd.startswith("QUIT"):
                self.reply("221 bye")
                return
            elif cmd.startswith(("MAIL", "RCPT", "RSET", "NOOP")):
                self.reply("250 ok")
            else:
                self.reply("502 not implemented")


class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


Server(("127.0.0.1", port), Handler).serve_forever()
