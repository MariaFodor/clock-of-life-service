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
    # Coerce to a real bool: callers pass truthy values (ids, dicts), and the summary sums these.
    checks.append((name, bool(ok), detail))
    print(f"  [{'PASS' if ok else 'FAIL'}] {name}" + (f" — {detail}" if detail else ""))


def get(path):
    with urllib.request.urlopen(BASE + path, timeout=5) as r:
        return r.status, json.loads(r.read())


def post(path, body, token=None):
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = "Bearer " + token
    req = urllib.request.Request(BASE + path, data=json.dumps(body).encode(), headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=5) as r:
            return r.status, json.loads(r.read())
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()


def get_auth(path, token):
    req = urllib.request.Request(BASE + path, headers={"Authorization": "Bearer " + token})
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

        # ── Bundle 7 (LEV): every answered lever reaches the number, the explanation, the overlay
        # and the advice — and an unanswered one changes nothing. ──────────────────────────────
        plain = {"country": "RO", "age": 55, "sex": "M", "smoke": 0, "pa_min": 600,
                 "sleep": 7, "waist": 95}
        _, base_est = post("/api/estimate", plain)
        base_years = base_est["estimate_years"]

        # 1. Unanswered levers and answers at their centring reference are the same number.
        at_reference = {**plain, "diet_score": 2.5, "sitting_hours": 6.0,
                        "stress_score": 6.11, "alcohol": "light"}
        _, ref_est = post("/api/estimate", at_reference)
        check("levers: unanswered == answered-at-reference (national anchoring holds)",
              ref_est["estimate_years"] == base_years,
              f'{ref_est["estimate_years"]} vs {base_years}')

        # 2. Each lever moves the estimate in the direction the evidence says.
        def years(extra):
            _, e = post("/api/estimate", {**plain, **extra})
            return e["estimate_years"]

        check("lever: heavy drinking costs years vs abstaining",
              years({"alcohol": "heavy"}) < years({"alcohol": "none"}),
              f'{years({"alcohol": "heavy"})} < {years({"alcohol": "none"})}')
        check("lever: a better diet outlives a worse one",
              years({"diet_score": 5}) > years({"diet_score": 0}),
              f'{years({"diet_score": 5})} > {years({"diet_score": 0})}')
        check("lever: heavy sitting costs years", years({"sitting_hours": 12}) < base_years)
        check("lever: high perceived stress costs years", years({"stress_score": 16}) < base_years)
        check("context: mobility difficulty lowers the estimate", years({"mobility": 1}) < base_years)
        check("env: a polluted, grey location costs years vs a clean, green one",
              years({"pm25": 25, "ndvi": 0.3}) < years({"pm25": 8, "ndvi": 0.7}))

        # 3. The levers explain themselves (why[]) at their real evidence grade.
        risky = {**plain, "alcohol": "heavy", "diet_score": 0, "sitting_hours": 12,
                 "stress_score": 14}
        _, risky_est = post("/api/estimate", risky)
        why = {w["key"]: w for w in risky_est["why"]}
        check("why[]: all four literature levers explain themselves",
              {"alcohol", "diet", "sedentary", "stress"} <= set(why),
              ", ".join(sorted(why)))
        check("why[]: each lever costs years and cites its evidence",
              all(why[k]["delta_years"] < 0 and why[k]["citation"]
                  for k in ("alcohol", "diet", "sedentary", "stress")))
        check("why[]: stress is shown at its honest (weak) grade",
              why["stress"]["evidence"] == "weak", why["stress"]["evidence"])

        # 4. What-If prices them, and agrees with scoring them directly.
        _, wi = post("/api/whatif", {"base": risky,
                                     "changes": {"alcohol": "none", "diet_score": 5,
                                                 "sitting_hours": 4, "stress_score": 4}})
        _, improved = post("/api/estimate", {**risky, "alcohol": "none", "diet_score": 5,
                                             "sitting_hours": 4, "stress_score": 4})
        direct = round(improved["estimate_years"] - risky_est["estimate_years"], 1)
        check("what-if: improving every lever adds years", wi["delta_years"] > 1.0,
              f'+{wi["delta_years"]}')
        check("what-if: the overlay agrees with scoring the change directly",
              abs(wi["delta_years"] - direct) <= 0.3, f'{wi["delta_years"]} vs {direct}')

        # 5. The advice targets them, with openable references — and never on an unanswered lever.
        _, recs = post("/api/recommendations", risky)
        by_feature = {r["feature"]: r for r in recs}
        check("recommendations: all four levers are actionable advice",
              {"alcohol", "diet", "sedentary", "stress"} <= set(by_feature),
              ", ".join(sorted(by_feature)))
        check("recommendations: each carries at least one openable study",
              all(by_feature[f]["references"] for f in ("alcohol", "diet", "sedentary", "stress")))
        _, plain_recs = post("/api/recommendations", plain)
        check("recommendations: an unanswered lever is never recommended",
              not ({"alcohol", "diet", "sedentary", "stress"} & {r["feature"] for r in plain_recs}))

        # 6. The full interview round-trips: register -> save every answer -> read them back.
        email = f"witness-{int(time.time())}@example.com"
        s, auth = post("/api/auth/register", {"email": email, "password": "witness-probe-pw"})
        token = auth.get("token") if s == 200 else None
        check("account: register returns a bearer token", bool(token), str(s))
        answers = [
            {"question_code": "Q1_age", "value": 55},
            {"question_code": "Q5_smoking", "value": "never"},
            {"question_code": "Q18_alcohol", "value": "heavy"},
            {"question_code": "Q19_stress", "value": [3, 1, 1, 3]},   # PSS-4 battery
            {"question_code": "Q20_mood", "value": [1, 0]},           # PHQ-2 battery
            {"question_code": "Q23_location", "value": {"name": "Cluj-Napoca", "country": "RO"}},
        ]
        s, saved = post("/api/answers", {"answers": answers}, token)
        check("interview: every answer shape the web sends is accepted",
              s == 200 and saved.get("saved") == len(answers), str(saved))
        # The second run is the witness: read the state back, not just write it.
        s, back = get_auth("/api/answers", token)
        stored = {a["question_code"]: a["value"] for a in back} if s == 200 else {}
        check("interview: answers read back identically (batteries stay arrays)",
              stored.get("Q19_stress") == [3, 1, 1, 3] and stored.get("Q20_mood") == [1, 0],
              str(stored.get("Q19_stress")))
        check("interview: the location answer keeps its object shape",
              (stored.get("Q23_location") or {}).get("name") == "Cluj-Napoca")
        s, _ = post("/api/profile/location", {"name": "Cluj-Napoca", "country": "RO"}, token)
        check("interview: the home location is accepted", s == 200, str(s))
        s, profile = get_auth("/api/profile", token)
        check("interview: the home location reads back on the profile",
              s == 200 and profile.get("home_location_id"), str(s))

        return finish()
    finally:
        srv.terminate()


def finish():
    ok = all(c[1] for c in checks)
    print(f"\nWITNESS: {'PASS' if ok else 'FAIL'} ({sum(c[1] for c in checks)}/{len(checks)} checks)")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
