#!/usr/bin/env python3
"""Hermetic AWS fixture for the aws-cloud-mcp e2e suite.

A threaded HTTP server that impersonates the five AWS endpoints behind one
base URL (routing by the SigV4 credential-scope service) and — unlike a stub —
verifies every signature: it rebuilds the canonical request from what it
received (method, raw path, re-encoded sorted query, the listed headers, the
body hash), derives the signing key with the shared test secret and compares.
A mismatch answers 403 SignatureDoesNotMatch in the service's own error
dialect, echoing the CanonicalRequest/StringToSign exactly like AWS does, so a
signer regression is debuggable from the test output.

Scenario hooks live in the access key (AKIABADKEY / AKIAEXPIRED / AKIASKEW)
and in resource names (bucket `nope`, function `boom`, log group `throttle`).
GET /_last (unsigned) returns the last verified request as JSON so tests can
assert encoding, clamping and signed headers.

Usage: fixture.py <port>
"""
import base64
import datetime
import hashlib
import hmac
import json
import re
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import quote, unquote, urlparse

SECRET = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
KEYS = {
    "AKIAIOSFODNN7EXAMPLE": SECRET,
    "AKIABADKEY": SECRET,
    "AKIAEXPIRED": SECRET,
    "AKIASKEW": SECRET,
    "ASIATEMPKEY": SECRET,
}
SKEW_SECS = 1200  # the AKIASKEW fixture clock runs 20 minutes ahead
LOCK = threading.Lock()
STATE = {"last": None, "count": 0, "objects": {}}
AUTH_RE = re.compile(
    r"^AWS4-HMAC-SHA256 Credential=([^/]+)/(\d{8})/([^/]+)/([^/]+)/aws4_request,\s*"
    r"SignedHeaders=([^,]+),\s*Signature=([0-9a-f]{64})$"
)
TEST_TXT = ("The quick brown fox jumps over the lazy dog. " * 10).encode()[:440] + b"end"
assert len(TEST_TXT) == 443
UNICODE_KEY = "photos/2006/Ünïcode ☃.jpg"
PLUS_KEY = "a b+c?d.txt"
BUILTIN_OBJECTS = {
    "test.txt": (TEST_TXT, "text/plain"),
    "binary.bin": (bytes([0xFF, 0xFE, 0x00, 0x80, 0xC3, 0x28]) * 4, "application/octet-stream"),
    "empty": (b"", "text/plain"),
    "docs/readme.md": (b"# readme\n", "text/markdown"),
    UNICODE_KEY: ("snowman ☃".encode(), "image/jpeg"),
    PLUS_KEY: (b"plus and question", "text/plain"),
    "multibyte.txt": (("☃" * 100).encode(), "text/plain; charset=utf-8"),
}
LIST_KEYS = ["a b+c?d.txt", "binary.bin", "docs/readme.md", "empty", "multibyte.txt",
             UNICODE_KEY, "test.txt"]


def uri_encode(s, encode_slash=True):
    return quote(s, safe="-_.~" + ("" if encode_slash else "/"))


def canonical_query(qs):
    if not qs:
        return ""
    pairs = []
    for part in qs.split("&"):
        k, _, v = part.partition("=")
        pairs.append((uri_encode(unquote(k)), uri_encode(unquote(v))))
    pairs.sort()
    return "&".join(f"{k}={v}" for k, v in pairs)


def httpdate(ts):
    return datetime.datetime.fromtimestamp(ts, datetime.timezone.utc).strftime("%a, %d %b %Y %H:%M:%S GMT")


def xml_escape(s):
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    # -- plumbing ---------------------------------------------------------
    def read_body(self):
        # wasmtime's outbound client may stream POST bodies chunked; a real
        # AWS endpoint handles both forms.
        if "chunked" in self.headers.get("Transfer-Encoding", "").lower():
            data = b""
            while True:
                line = self.rfile.readline().strip()
                if not line:
                    break
                size = int(line.split(b";")[0], 16)
                if size == 0:
                    while self.rfile.readline() not in (b"\r\n", b"\n", b""):
                        pass
                    break
                data += self.rfile.read(size)
                self.rfile.readline()
            return data
        length = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(length) if length else b""

    def send(self, status, body=b"", ctype="application/xml", headers=None):
        if isinstance(body, str):
            body = body.encode("utf-8")
        # send_response_only: send_response() would add its own Date header
        # (real time) ahead of the skewed one the clock-skew scenario needs.
        self.send_response_only(status)
        self.send_header("Server", "AmazonS3-fixture")
        headers = dict(headers or {})
        if not any(k.lower() == "date" for k in headers):
            headers["Date"] = httpdate(time.time())
        if body or status not in (204, 304):
            self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("x-amzn-RequestId", "fixture-req-1")
        for k, v in headers.items():
            self.send_header(k, v)
        self.end_headers()
        if body:
            self.wfile.write(body)

    # -- error dialects ---------------------------------------------------
    def error(self, service, status, code, message, headers=None, extra_xml=""):
        headers = dict(headers or {})
        if service == "s3":
            body = (f"<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>{code}</Code>"
                    f"<Message>{xml_escape(message)}</Message>{extra_xml}"
                    f"<RequestId>fixture-req-1</RequestId></Error>")
            return self.send(status, body, "application/xml", headers)
        if service == "sts":
            body = (f"<ErrorResponse xmlns=\"https://sts.amazonaws.com/doc/2011-06-15/\"><Error>"
                    f"<Type>Sender</Type><Code>{code}</Code><Message>{xml_escape(message)}</Message>"
                    f"</Error><RequestId>fixture-req-1</RequestId></ErrorResponse>")
            return self.send(status, body, "text/xml", headers)
        if service == "ec2":
            body = (f"<?xml version=\"1.0\" encoding=\"UTF-8\"?><Response><Errors><Error><Code>{code}</Code>"
                    f"<Message>{xml_escape(message)}</Message></Error></Errors>"
                    f"<RequestID>fixture-req-1</RequestID></Response>")
            return self.send(status, body, "text/xml", headers)
        if service == "lambda":
            headers["x-amzn-ErrorType"] = code
            body = json.dumps({"Message": message})
            if code == "TooManyRequestsException":
                body = json.dumps({"Reason": "CallerRateLimitExceeded", "Type": "User", "message": message})
            return self.send(status, body, "application/json", headers)
        headers["x-amzn-ErrorType"] = f"{code}:http://internal.amazon.com/coral/com.amazonaws.cloudwatchlogs/"
        body = json.dumps({"__type": code, "message": message})
        return self.send(status, body, "application/x-amz-json-1.1", headers)

    # -- SigV4 verification -------------------------------------------------
    def verify(self, method, raw_path, qs, body):
        """Returns the service name when the signature verifies, else answers
        the error and returns None."""
        auth = self.headers.get("Authorization", "")
        m = AUTH_RE.match(auth)
        if not m:
            self.error("s3", 403, "AccessDenied", f"missing or malformed Authorization header: {auth[:80]}")
            return None
        ak, date, region, service, signed_headers, sig = m.groups()
        if service not in ("sts", "s3", "ec2", "lambda", "logs"):
            self.error("s3", 400, "InvalidRequest", f"unknown service scope {service}")
            return None
        amz_date = self.headers.get("x-amz-date", "")
        try:
            req_ts = datetime.datetime.strptime(amz_date, "%Y%m%dT%H%M%SZ").replace(tzinfo=datetime.timezone.utc).timestamp()
        except ValueError:
            self.error(service, 400, "InvalidParameter", f"bad x-amz-date {amz_date!r}")
            return None
        now = time.time() + (SKEW_SECS if ak == "AKIASKEW" else 0)
        if abs(now - req_ts) > 900:
            hdr = {"Date": httpdate(now)}
            msg = f"The difference between the request time and the current time is too large: request {amz_date}, server {httpdate(now)}"
            if service == "s3":
                self.error(service, 403, "RequestTimeTooSkewed", msg, hdr)
            elif service == "sts":
                # Real STS phrasing (observed live): the code is SignatureDoesNotMatch.
                fmt = lambda ts: datetime.datetime.fromtimestamp(ts, datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
                self.error(service, 403, "SignatureDoesNotMatch",
                           f"Signature expired: {amz_date} is now earlier than {fmt(now - 900)} ({fmt(now)} - 15 min.)", hdr)
            elif service == "ec2":
                self.error(service, 400, "RequestExpired", msg, hdr)
            else:
                self.error(service, 403, "InvalidSignatureException", "Signature expired: " + msg, hdr)
            return None
        if amz_date[:8] != date:
            self.error(service, 403, "SignatureDoesNotMatch", "credential scope date does not match x-amz-date")
            return None
        if ak == "AKIABADKEY" or ak not in KEYS:
            if service == "sts":
                self.error(service, 403, "InvalidClientTokenId", "The security token included in the request is invalid.")
            elif service == "s3":
                self.error(service, 403, "InvalidAccessKeyId", "The AWS Access Key Id you provided does not exist in our records.")
            elif service == "ec2":
                self.error(service, 401, "AuthFailure", "AWS was not able to validate the provided access credentials")
            elif service == "lambda":
                self.error(service, 403, "UnrecognizedClientException", "The security token included in the request is invalid.")
            else:
                self.error(service, 400, "UnrecognizedClientException", "The security token included in the request is invalid.")
            return None
        if ak == "AKIAEXPIRED":
            if service == "s3":
                self.error(service, 400, "ExpiredToken", "The provided token has expired.")
            elif service in ("sts", "ec2"):
                self.error(service, 403, "ExpiredToken", "The security token included in the request is expired")
            else:
                self.error(service, 403, "ExpiredTokenException", "The security token included in the request is expired")
            return None
        if ak.startswith("ASIA") and not self.headers.get("x-amz-security-token"):
            self.error(service, 403, "InvalidToken", "temporary key without x-amz-security-token")
            return None
        sh = signed_headers.split(";")
        required = ["host", "x-amz-date"]
        if service == "s3":
            required.append("x-amz-content-sha256")
        if service == "logs":
            required += ["content-type", "x-amz-target"]
        if self.headers.get("x-amz-security-token"):
            required.append("x-amz-security-token")
        for h in self.headers.keys():
            if h.lower().startswith("x-amz-") and h.lower() != "x-amz-date":
                required.append(h.lower())
        missing = [h for h in required if h not in sh]
        if missing:
            self.error(service, 403, "SignatureDoesNotMatch", f"required headers not signed: {missing}")
            return None
        if sh != sorted(sh):
            self.error(service, 403, "SignatureDoesNotMatch", "SignedHeaders not sorted")
            return None
        payload_hash = hashlib.sha256(body).hexdigest()
        if service == "s3":
            sent = self.headers.get("x-amz-content-sha256", "")
            if sent != payload_hash:
                self.error(service, 400, "XAmzContentSHA256Mismatch",
                           f"x-amz-content-sha256 {sent} != sha256(body) {payload_hash}")
                return None
        canon_headers = ""
        for h in sh:
            value = self.headers.get(h)
            if value is None:
                self.error(service, 403, "SignatureDoesNotMatch", f"signed header {h} not present")
                return None
            canon_headers += f"{h}:{' '.join(value.split())}\n"
        canonical_uri = raw_path if service == "s3" else uri_encode(raw_path, encode_slash=False)
        cr = "\n".join([method, canonical_uri, canonical_query(qs), canon_headers, signed_headers, payload_hash])
        sts = "\n".join(["AWS4-HMAC-SHA256", amz_date, f"{date}/{region}/{service}/aws4_request",
                         hashlib.sha256(cr.encode()).hexdigest()])
        k = ("AWS4" + KEYS[ak]).encode()
        for part in (date, region, service, "aws4_request"):
            k = hmac.new(k, part.encode(), hashlib.sha256).digest()
        expected = hmac.new(k, sts.encode(), hashlib.sha256).hexdigest()
        if expected != sig:
            # Mirror the real dialects: S3 keeps its message short and echoes
            # <CanonicalRequest>/<StringToSign> as sibling elements; the Query
            # and JSON services embed "The Canonical String for this request
            # should have been ..." in the message itself. Both echoes carry
            # the x-amz-security-token header verbatim, exactly like AWS.
            if service == "s3":
                extra = (f"<CanonicalRequest>{xml_escape(cr)}</CanonicalRequest>"
                         f"<StringToSign>{xml_escape(sts)}</StringToSign>")
                self.error(service, 403, "SignatureDoesNotMatch",
                           "The request signature we calculated does not match the signature you provided. "
                           "Check your key and signing method.", extra_xml=extra)
                return None
            msg = ("The request signature we calculated does not match the signature you provided. "
                   "Check your AWS Secret Access Key and signing method. Consult the service "
                   "documentation for details.\n\nThe Canonical String for this request should have been\n"
                   f"'{cr}'\n\nThe String-to-Sign should have been\n'{sts}'\n")
            code = "SignatureDoesNotMatch" if service in ("sts", "ec2") else "InvalidSignatureException"
            self.error(service, 403, code, msg)
            return None
        with LOCK:
            STATE["count"] += 1
            STATE["last"] = {
                "method": method, "raw_path": raw_path, "query": qs,
                "headers": {k.lower(): v for k, v in self.headers.items()},
                "body": body[:8192].decode("utf-8", "replace"),
                "canonical_request": cr, "signed_headers": signed_headers,
                "service": service, "region": region, "access_key": ak,
            }
        return service

    # -- routing ----------------------------------------------------------
    def do_GET(self):
        self.handle_any("GET")

    def do_POST(self):
        self.handle_any("POST")

    def do_PUT(self):
        self.handle_any("PUT")

    def handle_any(self, method):
        u = urlparse(self.path)
        raw_path, qs = u.path, u.query
        if raw_path == "/_last":
            with LOCK:
                return self.send(200, json.dumps(STATE["last"] or {}), "application/json")
        if raw_path == "/_count":
            with LOCK:
                return self.send(200, json.dumps({"count": STATE["count"]}), "application/json")
        if raw_path == "/_hang":
            time.sleep(600)
            return
        body = self.read_body()
        service = self.verify(method, raw_path, qs, body)
        if service is None:
            return
        query = {}
        for part in qs.split("&") if qs else []:
            k, _, v = part.partition("=")
            query[unquote(k)] = unquote(v)
        try:
            getattr(self, "svc_" + service)(method, raw_path, query, body)
        except Exception as exc:  # never leave the client hanging
            self.error(service, 500, "InternalError", f"fixture bug: {exc!r}")

    # -- STS ---------------------------------------------------------------
    def svc_sts(self, method, path, query, body):
        form = dict(p.partition("=")[::2] for p in body.decode().split("&") if p)
        if form.get("Action") != "GetCallerIdentity":
            return self.error("sts", 400, "InvalidAction", f"unsupported action {form.get('Action')}")
        if self.headers.get("x-amz-security-token"):
            arn, uid = "arn:aws:sts::123456789012:assumed-role/e2e-role/session", "AROAEXAMPLE:session"
        else:
            arn, uid = "arn:aws:iam::123456789012:user/e2e", "AIDAEXAMPLE"
        xml = (f"<GetCallerIdentityResponse xmlns=\"https://sts.amazonaws.com/doc/2011-06-15/\">"
               f"<GetCallerIdentityResult><Arn>{arn}</Arn><UserId>{uid}</UserId><Account>123456789012</Account>"
               f"</GetCallerIdentityResult><ResponseMetadata><RequestId>fixture-req-1</RequestId>"
               f"</ResponseMetadata></GetCallerIdentityResponse>")
        self.send(200, xml, "text/xml")

    # -- S3 ----------------------------------------------------------------
    def svc_s3(self, method, path, query, body):
        parts = path.split("/", 2)  # ['', bucket, key]
        bucket = unquote(parts[1]) if len(parts) > 1 else ""
        key = unquote(parts[2]) if len(parts) > 2 else ""
        region = self.headers.get("Authorization").split("/")[2]
        if bucket == "":
            return self.s3_list_buckets(query)
        if bucket == "wrong-region" and region != "eu-west-1":
            return self.send(301, b"", "application/xml", {"x-amz-bucket-region": "eu-west-1"})
        if bucket == "nope":
            return self.error("s3", 404, "NoSuchBucket", "The specified bucket does not exist")
        if bucket == "denied":
            return self.error("s3", 403, "AccessDenied", "User: arn:aws:iam::123456789012:user/e2e is not authorized to perform: s3:ListBucket on resource: arn:aws:s3:::denied")
        if bucket == "slow":
            return self.error("s3", 503, "SlowDown", "Please reduce your request rate.")
        if key == "":
            if method != "GET" or query.get("list-type") != "2":
                return self.error("s3", 400, "InvalidRequest", "expected ListObjectsV2")
            return self.s3_list_objects(bucket, query)
        if method == "GET":
            return self.s3_get_object(bucket, key, query)
        if method == "PUT":
            return self.s3_put_object(bucket, key, body)
        return self.error("s3", 405, "MethodNotAllowed", method)

    def s3_list_buckets(self, query):
        buckets = [("alpha-bucket", "us-east-1"), ("beta.bucket", "eu-west-1"), ("gamma-bucket", "us-east-1")]
        prefix = query.get("prefix", "")
        buckets = [b for b in buckets if b[0].startswith(prefix)]
        mb = int(query.get("max-buckets", "10000"))
        token = ""
        if query.get("continuation-token") == "bkt-next":
            buckets = buckets[mb:]
        elif mb < len(buckets):
            buckets, token = buckets[:mb], "bkt-next"
        items = "".join(f"<Bucket><Name>{n}</Name><CreationDate>2020-01-0{i+1}T00:00:00.000Z</CreationDate>"
                        f"<BucketRegion>{r}</BucketRegion></Bucket>" for i, (n, r) in enumerate(buckets))
        xml = (f"<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListAllMyBucketsResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">"
               f"<Owner><ID>AIDAEXAMPLE</ID></Owner><Buckets>{items}</Buckets>"
               + (f"<Prefix>{xml_escape(prefix)}</Prefix>" if prefix else "")
               + (f"<ContinuationToken>{token}</ContinuationToken>" if token else "")
               + "</ListAllMyBucketsResult>")
        self.send(200, xml)

    def all_keys(self, bucket):
        with LOCK:
            stored = [k for (b, k) in STATE["objects"] if b == bucket]
        return sorted(set(LIST_KEYS + stored))

    def s3_list_objects(self, bucket, query):
        prefix = query.get("prefix", "")
        delimiter = query.get("delimiter", "")
        max_keys = int(query.get("max-keys", "1000"))
        start_after = query.get("start-after", "")
        keys = [k for k in self.all_keys(bucket) if k.startswith(prefix) and k > start_after]
        if query.get("continuation-token") == "obj-next":
            keys = keys[max_keys:]
        contents, prefixes = [], []
        for k in keys:
            rest = k[len(prefix):]
            if delimiter and delimiter in rest:
                p = prefix + rest.split(delimiter, 1)[0] + delimiter
                if p not in prefixes:
                    prefixes.append(p)
            else:
                contents.append(k)
        entries = contents + prefixes
        truncated = len(entries) > max_keys
        entries = entries[:max_keys]
        contents = [k for k in contents if k in entries]
        prefixes = [p for p in prefixes if p in entries]
        items = ""
        for k in contents:
            with LOCK:
                stored = STATE["objects"].get((bucket, k))
            data = stored[0] if stored else BUILTIN_OBJECTS.get(k, (b"x", ""))[0]
            items += (f"<Contents><Key>{xml_escape(k)}</Key><LastModified>2024-05-01T12:00:00.000Z</LastModified>"
                      f"<ETag>&quot;{hashlib.md5(data).hexdigest()}&quot;</ETag><Size>{len(data)}</Size>"
                      f"<StorageClass>STANDARD</StorageClass></Contents>")
        for p in prefixes:
            items += f"<CommonPrefixes><Prefix>{xml_escape(p)}</Prefix></CommonPrefixes>"
        xml = (f"<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">"
               f"<Name>{bucket}</Name><Prefix>{xml_escape(prefix)}</Prefix>"
               + (f"<Delimiter>{xml_escape(delimiter)}</Delimiter>" if delimiter else "")
               + (f"<StartAfter>{xml_escape(start_after)}</StartAfter>" if start_after else "")
               + (f"<ContinuationToken>{query['continuation-token']}</ContinuationToken>" if query.get("continuation-token") else "")
               + f"<MaxKeys>{max_keys}</MaxKeys><KeyCount>{len(entries)}</KeyCount>"
               f"<IsTruncated>{'true' if truncated else 'false'}</IsTruncated>"
               + ("<NextContinuationToken>obj-next</NextContinuationToken>" if truncated else "")
               + items + "</ListBucketResult>")
        self.send(200, xml)

    def s3_get_object(self, bucket, key, query):
        if key == "missing":
            return self.error("s3", 404, "NoSuchKey", "The specified key does not exist.")
        if key == "glacier":
            return self.error("s3", 403, "InvalidObjectState", "The operation is not valid for the object's storage class")
        with LOCK:
            stored = STATE["objects"].get((bucket, key))
        if stored:
            data, ctype = stored
        elif key in BUILTIN_OBJECTS:
            data, ctype = BUILTIN_OBJECTS[key]
        else:
            return self.error("s3", 404, "NoSuchKey", "The specified key does not exist.")
        headers = {"ETag": f"\"{hashlib.md5(data).hexdigest()}\"", "Last-Modified": "Wed, 01 May 2024 12:00:00 GMT",
                   "Accept-Ranges": "bytes"}
        if query.get("versionId"):
            headers["x-amz-version-id"] = query["versionId"]
        rng = self.headers.get("Range")
        if rng:
            m = re.match(r"bytes=(\d+)-(\d+)$", rng)
            if not m:
                return self.error("s3", 400, "InvalidArgument", "bad Range")
            start, end = int(m.group(1)), int(m.group(2))
            if len(data) == 0 or start >= len(data):
                return self.error("s3", 416, "InvalidRange", "The requested range is not satisfiable")
            end = min(end, len(data) - 1)
            headers["Content-Range"] = f"bytes {start}-{end}/{len(data)}"
            return self.send(206, data[start:end + 1], ctype, headers)
        self.send(200, data, ctype, headers)

    def s3_put_object(self, bucket, key, body):
        if bucket == "denied":
            return self.error("s3", 403, "AccessDenied", "Access Denied")
        with LOCK:
            exists = (bucket, key) in STATE["objects"] or key in BUILTIN_OBJECTS
        if self.headers.get("If-None-Match") == "*" and exists:
            return self.error("s3", 412, "PreconditionFailed", "At least one of the pre-conditions you specified did not hold")
        ctype = self.headers.get("Content-Type", "binary/octet-stream")
        with LOCK:
            STATE["objects"][(bucket, key)] = (body, ctype)
        headers = {"ETag": f"\"{hashlib.md5(body).hexdigest()}\""}
        if bucket == "versioned":
            headers["x-amz-version-id"] = "v1fixture"
        self.send(200, b"", "application/xml", headers)

    # -- EC2 ---------------------------------------------------------------
    def svc_ec2(self, method, path, query, body):
        form = {}
        for p in body.decode().split("&"):
            if p:
                k, _, v = p.partition("=")
                form[unquote(k)] = unquote(v)
        if form.get("Action") != "DescribeInstances":
            return self.error("ec2", 400, "InvalidAction", f"unsupported action {form.get('Action')}")
        ids = [v for k, v in form.items() if k.startswith("InstanceId.")]
        if ids and "MaxResults" in form:
            return self.error("ec2", 400, "InvalidParameterCombination", "The parameter instancesSet cannot be used with the parameter maxResults")
        for i in ids:
            if not re.match(r"^i-[0-9a-f]{8}([0-9a-f]{9})?$", i):
                return self.error("ec2", 400, "InvalidInstanceID.Malformed", f"Invalid id: \"{i}\"")
            if i == "i-0000000000000000f":
                return self.error("ec2", 400, "InvalidInstanceID.NotFound", f"The instance ID '{i}' does not exist")
        instances = [
            ("i-0abc123def4567890", "running", "t3.micro", "us-east-1a", "10.0.1.10", "54.1.2.3", "web-1"),
            ("i-0fedcba9876543210", "stopped", "m5.large", "us-east-1b", "10.0.2.20", None, None),
        ]
        states = [v for k, v in form.items() if k.startswith("Filter.") and k.endswith(".Name") and v == "instance-state-name"]
        if states:
            idx = [k.split(".")[1] for k, v in form.items() if k.endswith(".Name") and v == "instance-state-name"][0]
            wanted = [v for k, v in form.items() if k.startswith(f"Filter.{idx}.Value.")]
            instances = [i for i in instances if i[1] in wanted]
        if ids:
            instances = [i for i in instances if i[0] in ids]
        items = ""
        for (iid, state, itype, az, priv, pub, name) in instances:
            tags = f"<tagSet><item><key>Name</key><value>{name}</value></item><item><key>env</key><value>e2e &lt;x&gt;</value></item></tagSet>" if name else "<tagSet/>"
            items += (f"<item><reservationId>r-{iid[2:10]}</reservationId><ownerId>123456789012</ownerId><instancesSet><item>"
                      f"<instanceId>{iid}</instanceId><imageId>ami-0123456789abcdef0</imageId>"
                      f"<instanceState><code>16</code><name>{state}</name></instanceState><instanceType>{itype}</instanceType>"
                      f"<launchTime>2024-04-01T08:00:00.000Z</launchTime><placement><availabilityZone>{az}</availabilityZone></placement>"
                      f"<subnetId>subnet-0aa</subnetId><vpcId>vpc-0bb</vpcId><privateIpAddress>{priv}</privateIpAddress>"
                      + (f"<ipAddress>{pub}</ipAddress>" if pub else "") + tags + "</item></instancesSet></item>")
        next_token = "<nextToken>next-2</nextToken>" if ("MaxResults" in form and "NextToken" not in form) else ""
        xml = (f"<?xml version=\"1.0\" encoding=\"UTF-8\"?><DescribeInstancesResponse xmlns=\"http://ec2.amazonaws.com/doc/2016-11-15/\">"
               f"<requestId>fixture-req-1</requestId><reservationSet>{items}</reservationSet>{next_token}</DescribeInstancesResponse>")
        self.send(200, xml, "text/xml")

    # -- Lambda --------------------------------------------------------------
    def svc_lambda(self, method, path, query, body):
        if path == "/2015-03-31/functions" and method == "GET":
            fns = [
                {"FunctionName": "echo", "FunctionArn": "arn:aws:lambda:us-east-1:123456789012:function:echo",
                 "Runtime": "python3.12", "Handler": "app.handler", "MemorySize": 128, "Timeout": 3,
                 "LastModified": "2024-04-01T08:00:00.000+0000", "Description": "echoes", "PackageType": "Zip",
                 "Architectures": ["arm64"], "Version": "$LATEST", "CodeSize": 1234},
                {"FunctionName": "boom", "FunctionArn": "arn:aws:lambda:us-east-1:123456789012:function:boom",
                 "Runtime": "nodejs20.x", "Handler": "index.handler", "MemorySize": 512, "Timeout": 30,
                 "LastModified": "2024-04-02T08:00:00.000+0000", "Description": "", "PackageType": "Zip",
                 "Architectures": ["x86_64"], "Version": "$LATEST", "CodeSize": 99},
            ]
            max_items = int(query.get("MaxItems", "50"))
            out = {"Functions": fns[:max_items]}
            if query.get("Marker") == "mk-2":
                out = {"Functions": fns[max_items:]}
            elif max_items < len(fns):
                out["NextMarker"] = "mk-2"
            if query.get("FunctionVersion") == "ALL":
                out["Functions"] = out["Functions"] + [dict(fns[0], Version="1")]
            return self.send(200, json.dumps(out), "application/json")
        m = re.match(r"^/2015-03-31/functions/([^/]+)/invocations$", path)
        if not m or method != "POST":
            return self.error("lambda", 404, "ResourceNotFoundException", f"no route {method} {path}")
        name = unquote(m.group(1))
        itype = self.headers.get("X-Amz-Invocation-Type", "RequestResponse")
        if name == "missing":
            return self.error("lambda", 404, "ResourceNotFoundException", f"Function not found: arn:aws:lambda:us-east-1:123456789012:function:{name}")
        if name == "throttle":
            return self.error("lambda", 429, "TooManyRequestsException", "Rate Exceeded.", {"Retry-After": "1"})
        headers = {"X-Amz-Executed-Version": query.get("Qualifier", "$LATEST")}
        if itype == "DryRun":
            return self.send(204, b"", "application/json", headers)
        if itype == "Event":
            return self.send(202, b"", "application/json", headers)
        if self.headers.get("X-Amz-Log-Type") == "Tail":
            headers["X-Amz-Log-Result"] = base64.b64encode(
                f"START RequestId: fixture-req-1 Version: $LATEST\nname={name} bytes={len(body)}\nEND RequestId: fixture-req-1\n".encode()).decode()
        if name == "boom":
            headers["X-Amz-Function-Error"] = "Unhandled"
            return self.send(200, json.dumps({"errorMessage": "boom went off", "errorType": "RuntimeError",
                                              "stackTrace": ["File \"app.py\", line 1"]}), "application/json", headers)
        self.send(200, body or b"null", "application/json", headers)

    # -- CloudWatch Logs ---------------------------------------------------
    def svc_logs(self, method, path, query, body):
        target = self.headers.get("X-Amz-Target", "")
        if self.headers.get("Content-Type") != "application/x-amz-json-1.1":
            return self.error("logs", 400, "InvalidParameterException", f"bad content-type {self.headers.get('Content-Type')}")
        req = json.loads(body or b"{}")
        if target == "Logs_20140328.DescribeLogGroups":
            groups = [
                {"logGroupName": "/aws/lambda/echo", "arn": "arn:aws:logs:us-east-1:123456789012:log-group:/aws/lambda/echo:*",
                 "creationTime": 1712000000000, "retentionInDays": 14, "storedBytes": 2048, "logGroupClass": "STANDARD"},
                {"logGroupName": "/aws/lambda/boom", "arn": "arn:aws:logs:us-east-1:123456789012:log-group:/aws/lambda/boom:*",
                 "creationTime": 1712000001000, "storedBytes": 0, "logGroupClass": "STANDARD"},
                {"logGroupName": "app/prod", "arn": "arn:aws:logs:us-east-1:123456789012:log-group:app/prod:*",
                 "creationTime": 1712000002000, "retentionInDays": 30, "storedBytes": 4096, "logGroupClass": "INFREQUENT_ACCESS"},
            ]
            if "logGroupNamePrefix" in req and "logGroupNamePattern" in req:
                return self.error("logs", 400, "InvalidParameterException", "logGroupNamePrefix and logGroupNamePattern are mutually exclusive")
            if req.get("logGroupNamePrefix"):
                groups = [g for g in groups if g["logGroupName"].startswith(req["logGroupNamePrefix"])]
            if req.get("logGroupNamePattern"):
                groups = [{k: g[k] for k in ("logGroupName", "arn", "creationTime")} for g in groups if req["logGroupNamePattern"] in g["logGroupName"]]
            if req.get("logGroupClass"):
                groups = [g for g in groups if g.get("logGroupClass") == req["logGroupClass"]]
            limit = int(req.get("limit", 50))
            out = {"logGroups": groups[:limit]}
            if req.get("nextToken") == "lg-next":
                out = {"logGroups": groups[limit:]}
            elif limit < len(groups):
                out["nextToken"] = "lg-next"
            return self.send(200, json.dumps(out), "application/x-amz-json-1.1")
        if target == "Logs_20140328.FilterLogEvents":
            group = req.get("logGroupName") or req.get("logGroupIdentifier") or ""
            if "logGroupName" in req and "logGroupIdentifier" in req:
                return self.error("logs", 400, "InvalidParameterException", "both logGroupName and logGroupIdentifier")
            if group.endswith("missing"):
                return self.error("logs", 400, "ResourceNotFoundException", "The specified log group does not exist.")
            if group.endswith("throttle"):
                return self.error("logs", 400, "ThrottlingException", "Rate exceeded")
            if group.endswith("empty-page"):
                return self.send(200, json.dumps({"events": [], "nextToken": "more"}), "application/x-amz-json-1.1")
            if "logStreamNamePrefix" in req and "logStreamNames" in req:
                return self.error("logs", 400, "InvalidParameterException", "logStreamNamePrefix and logStreamNames are mutually exclusive")
            echo = {k: req.get(k) for k in ("filterPattern", "startTime", "endTime", "limit", "logStreamNamePrefix", "logStreamNames", "startFromHead", "nextToken")}
            events = [{"timestamp": 1714564800000 + i * 1000, "ingestionTime": 1714564801000 + i * 1000,
                       "message": f"event {i} {json.dumps(echo, sort_keys=True)}", "logStreamName": f"stream-{i}",
                       "eventId": f"3113262927494551977980532285720373558671445464339159450{i}"} for i in range(3)]
            limit = int(req.get("limit", 10000))
            out = {"events": events[:limit], "searchedLogStreams": []}
            if req.get("nextToken") == "ev-next":
                out["events"] = events[limit:]
            elif limit < len(events):
                out["nextToken"] = "ev-next"
            return self.send(200, json.dumps(out), "application/x-amz-json-1.1")
        return self.error("logs", 400, "InvalidAction", f"unknown target {target}")


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
