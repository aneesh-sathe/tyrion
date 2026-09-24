//! Pieces shared by every Docker containment boundary Tyrion creates:
//! Worker sandboxes and one-shot credentialed Effect Sandboxes alike.

/// A destination-pinned TCP relay. It forwards bytes to exactly one
/// `host:port` and never terminates TLS, so a Worker's provider credential
/// stays end-to-end encrypted and cannot be sent anywhere else.
pub(crate) const RELAY_SOURCE: &str = r#"
import socket, sys, threading
host, port = sys.argv[1], int(sys.argv[2])
def pipe(source, sink):
    try:
        while True:
            block = source.recv(65536)
            if not block:
                break
            sink.sendall(block)
    except OSError:
        pass
    finally:
        try:
            sink.shutdown(socket.SHUT_WR)
        except OSError:
            pass
listener = socket.socket()
listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
listener.bind(("0.0.0.0", port))
listener.listen(64)
sys.stderr.write("tyrion-relay-ready\n")
sys.stderr.flush()
while True:
    client, _ = listener.accept()
    try:
        upstream = socket.create_connection((host, port), 15)
    except OSError:
        client.close()
        continue
    threading.Thread(target=pipe, args=(client, upstream), daemon=True).start()
    threading.Thread(target=pipe, args=(upstream, client), daemon=True).start()
"#;
