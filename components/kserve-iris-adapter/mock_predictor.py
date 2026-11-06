"""Local protocol fixture. It performs no model inference."""
import json
from http.server import BaseHTTPRequestHandler, HTTPServer


class Predictor(BaseHTTPRequestHandler):
    def do_POST(self):
        if self.path != "/v2/models/sklearn-iris/infer":
            self.send_error(404)
            return
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        tensor = request["inputs"][0]
        assert tensor["name"] == "input-0" and tensor["datatype"] == "FP32"
        rows, width = tensor["shape"]
        assert width == 4 and len(tensor["data"]) == rows * width
        payload = json.dumps({"model_name": "sklearn-iris", "outputs": [
            {"name": "predict", "datatype": "INT64", "shape": [rows], "data": [0] * rows}
        ]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


if __name__ == "__main__":
    HTTPServer(("127.0.0.1", 8081), Predictor).serve_forever()
