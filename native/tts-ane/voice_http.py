"""Loopback diagnostic text-to-PCM streaming; not a hardened public server."""
import json
from http.server import BaseHTTPRequestHandler, HTTPServer


def serve_voice(voice,port,max_frames):
    class Handler(BaseHTTPRequestHandler):
        protocol_version = 'HTTP/1.1'

        def setup(self):
            super().setup()
            self.connection.settimeout(30)

        def respond_stream(self,stream):
            headers_sent = False
            try:
                first,metadata = next(stream)
                self.send_response(200)
                self.send_header('Content-Type','audio/pcm; rate=24000; channels=1; format=s16le')
                self.send_header('Transfer-Encoding','chunked')
                self.send_header('Cache-Control','no-store')
                if 'request_ready_ms' in metadata:
                    self.send_header('X-TTS-First-PCM-Ms',str(metadata['request_ready_ms']))
                    self.send_header('X-TTS-Preparation-Ms',str(metadata['preparation_ms']))
                self.end_headers()
                headers_sent = True
                self.write_chunk(first)
                for payload,_ in stream:
                    self.write_chunk(payload)
                self.wfile.write(b'0\r\n\r\n')
                self.wfile.flush()
            except (BrokenPipeError,ConnectionResetError):
                self.close_connection = True
            except (ValueError,RuntimeError,StopIteration) as error:
                self.log_error('Synthesis failed: %s',error)
                if not headers_sent:
                    self.send_error(500,'Synthesis failed')
                self.close_connection = True
            finally:
                stream.close()

        def write_chunk(self,payload):
            self.wfile.write(f'{len(payload):X}\r\n'.encode('ascii')+payload+b'\r\n')
            self.wfile.flush()

        def do_GET(self):
            if self.path != '/stream':
                self.send_error(404)
                return
            self.respond_stream(voice.chunks(max_frames))

        def do_POST(self):
            if self.path != '/stream':
                self.send_error(404)
                return
            if voice.prefix_states is not None:
                self.send_error(409,'Disable cross-request prefix reuse for fresh text')
                return
            try:
                if self.headers.get('Transfer-Encoding'):
                    raise ValueError('Use a bounded Content-Length JSON request')
                length = int(self.headers.get('Content-Length','0'))
                if not 1 <= length <= 131072:
                    raise ValueError('JSON body must be 1..131072 bytes')
                body = json.loads(self.rfile.read(length).decode('utf-8'))
                if not isinstance(body,dict) or set(body)-{'text','max_frames'}:
                    raise ValueError('Expected text and optional max_frames')
                text = body.get('text')
                frames = body.get('max_frames',max_frames)
                if not isinstance(text,str) or not text.strip() or len(text)>32768:
                    raise ValueError('Text must contain 1..32768 characters and not be blank')
                if type(frames) is not int or not 1 <= frames <= voice.capacity-10:
                    raise ValueError('Invalid frame limit for the configured model')
            except (ValueError,UnicodeError) as error:
                self.send_error(400,str(error))
                return
            self.respond_stream(voice.chunks_for_text(text,frames))

    print(f'Fresh text POST http://127.0.0.1:{port}/stream; GET uses configured text',flush=True)
    HTTPServer(('127.0.0.1',port),Handler).serve_forever()
