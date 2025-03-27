#!/usr/bin/env python3

import http.server
import socketserver
import os
import urllib.parse
import subprocess
from functools import partial

class AuthHandler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *args, auth_token=None, **kwargs):
        self.auth_token = auth_token
        super().__init__(*args, **kwargs)

    def do_GET(self):
        # Parse URL and query parameters
        parsed = urllib.parse.urlparse(self.path)
        query = urllib.parse.parse_qs(parsed.query)
        
        # Check authentication for all paths except CSS/JS resources
        if not parsed.path.endswith(('.css', '.js')):
            auth = query.get('auth', [None])[0]
            if not auth or auth != self.auth_token:
                self.send_error(403, "Authentication required!!!")
                return

        # Strip auth parameter from filename query
        if 'filename' in query:
            filename = query['filename'][0]
            timestamp = query.get('timestamp', ['0'])[0]
            # Reconstruct path without auth parameter
            self.path = f"{parsed.path}?filename={filename}&timestamp={timestamp}"

        return super().do_GET()

def run_server(port, directory, auth_token):
    handler = partial(AuthHandler, auth_token=auth_token, directory=directory)
    ip = subprocess.check_output("ip route get 1 | awk '{print $7}'", shell=True).decode().strip()
    with socketserver.TCPServer((ip, port), handler) as httpd:
        print(f"Serving at port {port} with authentication")
        httpd.serve_forever()

if __name__ == "__main__":
    port = int(os.environ.get('CINELOG_VIEWER_PORT', 10000))
    auth_token = os.environ.get('CINELOG_AUTH_UUID')
    directory = os.path.expanduser("~/.zim/modules/cinelog/asciinema-player")
    
    if not auth_token:
        print("Error: CINELOG_AUTH_UUID environment variable not set")
        exit(1)
    
    run_server(port, directory, auth_token)