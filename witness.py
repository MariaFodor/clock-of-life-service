#!/usr/bin/env python3
"""Committed end-to-end probe for clock-of-life-service (AGENTS principle 2).

Builds + starts the service, drives every endpoint over real HTTP, checks the results against the model's
witnessed behaviour, prints a human-readable verdict, and returns non-zero on any failure.

    python3 witness.py
"""
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request

BASE = "http://127.0.0.1:8080"
checks = []


def check(name, ok, detail=""):
    checks.append((name, ok, detail))
    print(f"  [{'PASS' if ok else 'FAIL'}] {name}" + (f" — {detail}" if detail else ""))


def get(path):
    with urllib.request.urlopen(BASE + path, timeout=5) as r:
        return r.status, json.loads(r.read())


def post(path, body):
    req = urllib.request.Request(BASE + path, data=json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=5) as r:
            return r.status, json.loads(r.read())
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()


def main():
    subprocess.run(["cargo", "build", "--quiet"], check=True)
    # The service fails closed without a JWT secret (REVIEW-2026-09-09 S5) — the probe supplies one.
    env = dict(os.environ)
    env.setdefault("JWT_SECRET", "witness-probe-secret")
    srv = subprocess.Popen(["./target/debug/clock-of-life-service"],
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=env)
    try:
        # wait for liveness
        for _ in range(50):
            try:
                if get("/health")[0] == 200:
                    break
            except Exception:
                time.sleep(0.1)
        else:
            check("service starts", False, "never became healthy"); return finish()

        check("GET /health 200", get("/health")[0] == 200)

        s, meta = get("/api/meta")
        check("GET /api/meta model 2.2.0", meta.get("model_version") == "2.2.0", str(meta.get("model_version")))
        check("meta lists 30 countries", len(meta.get("countries", [])) == 30, str(len(meta.get("countries", []))))

        ro = {"country": "RO", "age": 40, "sex": "M", "smoke": 0, "pa_min": 2000, "sleep": 7,
              "waist": 85, "higher_educ": True, "income": 4.0}
        hi = {"country": "RO", "age": 40, "sex": "M", "smoke": 2, "pa_min": 0, "sleep": 9,
              "waist": 115, "diabetes": True, "high_bp": True, "income": 1.0}
        _, e_healthy = post("/api/estimate", ro)
        _, e_high = post("/api/estimate", hi)
        check("estimate: healthy outlives high-risk by >10y",
              e_healthy["estimate_years"] - e_high["estimate_years"] > 10,
              f'{e_healthy["estimate_years"]} vs {e_high["estimate_years"]}')
        check("estimate: healthy RR < 1 < high-risk RR",
              e_healthy["relative_risk"] < 1.0 < e_high["relative_risk"],
              f'{e_healthy["relative_risk"]} / {e_high["relative_risk"]}')

        smoker = {"country": "RO", "age": 45, "sex": "M", "smoke": 2, "pa_min": 100, "sleep": 7,
                  "waist": 108, "income": 2.0}
        _, quit = post("/api/whatif", {"base": smoker, "changes": {"smoke": 1}})
        check("whatif: quitting smoking adds years", quit["delta_years"] > 0, f'+{quit["delta_years"]}')
        check("whatif: cessation note present", "note" in quit)
        _, exercise = post("/api/whatif", {"base": smoker, "changes": {"pa_min": 2000}})
        check("whatif: exercising adds years", exercise["delta_years"] > 0, f'+{exercise["delta_years"]}')

        # sanity: an average national resident ~ national life expectancy (meta assumption holds)
        avg = {"country": "RO", "age": 40, "sex": "M", "smoke": 0, "pa_min": 300, "sleep": 7, "waist": 98}
        _, e_avg = post("/api/estimate", avg)
        check("estimate: RO ~40yo male reaches a plausible age (72-90)",
              72 <= e_avg["reaches_age"] <= 90, f'reaches {e_avg["reaches_age"]}')

        # input validation -> 400
        s, _ = post("/api/estimate", {**ro, "pa_min": -5})
        check("bad input -> 400", s == 400, str(s))

        return finish()
    finally:
        srv.terminate()


def finish():
    ok = all(c[1] for c in checks)
    print(f"\nWITNESS: {'PASS' if ok else 'FAIL'} ({sum(c[1] for c in checks)}/{len(checks)} checks)")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
