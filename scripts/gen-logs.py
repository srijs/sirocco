"""Writes the `logs` corpus set: server logs, a JSON log and an access log,
generated from a fixed seed so that every run makes the same files.

    python3 scripts/gen-logs.py DIR

Logs are a large part of what gets compressed with DEFLATE, and unlike the
classic corpora they are made of long repeats (keys, paths, user agents)
with short random fields between them.
"""
import json
import random
import sys

out = sys.argv[1]
random.seed(7)
users = [f"user{i}" for i in range(500)]
paths = ["/api/v1/orders", "/api/v1/users", "/static/app.js", "/health",
         "/api/v2/search", "/login", "/api/v1/cart/items"]
agents = ["Mozilla/5.0 (X11; Linux x86_64)", "curl/8.1.2",
          "Mozilla/5.0 (Macintosh; Intel Mac OS X 13_4)", "python-requests/2.31"]
t = 1700000000.0
with open(f"{out}/app.jsonl", "w") as f:
    for _ in range(120000):
        t += random.expovariate(20)
        f.write(json.dumps({
            "ts": round(t, 3),
            "level": random.choice(["INFO"] * 8 + ["WARN", "ERROR"]),
            "user": random.choice(users),
            "method": random.choice(["GET"] * 4 + ["POST", "PUT"]),
            "path": random.choice(paths),
            "status": random.choice([200] * 9 + [404, 500]),
            "ms": round(random.lognormvariate(3, 1), 2),
            "ua": random.choice(agents),
            "req": "%016x" % random.getrandbits(64),
        }) + "\n")
with open(f"{out}/access.log", "w") as f:
    for _ in range(150000):
        t += random.expovariate(20)
        ip = [random.randint(0, 255) for _ in range(3)]
        method = random.choice(["GET", "POST"])
        path = random.choice(paths)
        if random.random() < 0.5:
            path += "?id=%d" % random.randint(1, 99999)
        f.write('10.%d.%d.%d - - [%d] "%s %s HTTP/1.1" %d %d "-" "%s"\n' % (
            *ip, int(t), method, path, random.choice([200] * 9 + [404]),
            random.randint(100, 90000), random.choice(agents)))
