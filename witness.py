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

import os
BASE = os.environ.get("CLOCK_BASE", "http://127.0.0.1:8080")   # matches the service's CLOCK_ADDR
checks = []


def check(name, ok, detail=""):
    # Coerce to a real bool: callers pass truthy values (ids, dicts), and the summary sums these.
    checks.append((name, bool(ok), detail))
    print(f"  [{'PASS' if ok else 'FAIL'}] {name}" + (f" — {detail}" if detail else ""))


def _json_or_empty(status, raw):
    """Always hand back a mapping: an error body is a string, and resp["key"] on it would raise a
    TypeError mid-run — turning the very regression a check names into a crash instead of a FAIL."""
    try:
        body = json.loads(raw)
        return status, body if isinstance(body, (dict, list)) else {}
    except (ValueError, TypeError):
        return status, {}


def get(path):
    try:
        with urllib.request.urlopen(BASE + path, timeout=5) as r:
            return _json_or_empty(r.status, r.read())
    except urllib.error.HTTPError as e:
        return _json_or_empty(e.code, e.read())


def post(path, body, token=None):
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = "Bearer " + token
    req = urllib.request.Request(BASE + path, data=json.dumps(body).encode(), headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=5) as r:
            return _json_or_empty(r.status, r.read())
    except urllib.error.HTTPError as e:
        return _json_or_empty(e.code, e.read())


def get_auth(path, token, method="GET"):
    # A missing token must FAIL a check, never crash the probe on "Bearer " + None.
    if not token:
        return 0, {}
    req = urllib.request.Request(BASE + path, method=method,
                                 headers={"Authorization": "Bearer " + token})
    try:
        with urllib.request.urlopen(req, timeout=5) as r:
            return _json_or_empty(r.status, r.read())
    except urllib.error.HTTPError as e:
        return _json_or_empty(e.code, e.read())


def main():
    subprocess.run(["cargo", "build", "--quiet"], check=True)
    # The service fails closed without a JWT secret (REVIEW-2026-09-09 S5) — the probe supplies one.
    env = dict(os.environ)
    env.setdefault("JWT_SECRET", "witness-probe-secret")
    srv = subprocess.Popen(["./target/debug/clock-of-life-service"],
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=env)
    try:
        # wait for liveness. The sleep is in the loop body, not an except: get() reports HTTP
        # errors as a status now, so a listening-but-degraded service (503 on the db ping) would
        # otherwise spin through all 50 attempts in milliseconds.
        for _ in range(50):
            try:
                if get("/health")[0] == 200:
                    break
            except Exception:
                pass
            time.sleep(0.1)
        else:
            check("service starts", False, "never became healthy"); return finish()

        check("GET /health 200", get("/health")[0] == 200)

        s, meta = get("/api/meta")
        check("GET /api/meta model 4.0.0", meta.get("model_version") == "4.0.0", str(meta.get("model_version")))
        # Still 30, and that is the point: the bundle now carries 237 life tables so the atlas can
        # draw the world, while /api/meta keeps promising only the countries that can be CENTRED.
        # If this ever reads 237, every non-European user is being scored against a US cohort mean.
        check("meta still promises only the 30 countries that can be scored",
              len(meta.get("countries", [])) == 30, str(len(meta.get("countries", []))))
        el_status, _ = post("/api/estimate", {"country": "EL", "age": 50, "sex": "F", "smoke": 1,
                                             "pa_min": 200, "sleep": 7, "waist": 85})
        check("Greece still answers to the code Eurostat used (EL -> GR)", el_status == 200,
              str(el_status))
        # A country the map can draw but the clock cannot centre must be refused a personal number.
        ng_status, _ = post("/api/estimate", {"country": "NG", "age": 45, "sex": "M", "smoke": 0,
                                             "pa_min": 300, "sleep": 7, "waist": 90})
        check("a reference-only country is refused a personal estimate", ng_status == 400,
              str(ng_status))

        ro = {"country": "RO", "age": 40, "sex": "M", "smoke": 0, "pa_min": 2000, "sleep": 7,
              "waist": 85, "higher_educ": True, "income": 4.0}
        hi = {"country": "RO", "age": 40, "sex": "M", "smoke": 2, "pa_min": 0, "sleep": 9,
              "waist": 115, "diabetes": True, "high_bp": True, "income": 1.0}
        _, e_healthy = post("/api/estimate", ro)
        _, e_high = post("/api/estimate", hi)
        healthy_y, high_y = e_healthy.get("estimate_years"), e_high.get("estimate_years")
        check("estimate: healthy outlives high-risk by >10y",
              bool(healthy_y and high_y and healthy_y - high_y > 10), f"{healthy_y} vs {high_y}")
        healthy_rr, high_rr = e_healthy.get("relative_risk"), e_high.get("relative_risk")
        check("estimate: healthy RR < 1 < high-risk RR",
              bool(healthy_rr and high_rr and healthy_rr < 1.0 < high_rr),
              f"{healthy_rr} / {high_rr}")

        smoker = {"country": "RO", "age": 45, "sex": "M", "smoke": 2, "pa_min": 100, "sleep": 7,
                  "waist": 108, "income": 2.0}
        _, quit = post("/api/whatif", {"base": smoker, "changes": {"smoke": 1}})
        check("whatif: quitting smoking adds years", quit.get("delta_years", 0) > 0,
              f'+{quit.get("delta_years")}')
        check("whatif: cessation note present", "note" in quit)
        _, exercise = post("/api/whatif", {"base": smoker, "changes": {"pa_min": 2000}})
        check("whatif: exercising adds years", exercise.get("delta_years", 0) > 0,
              f'+{exercise.get("delta_years")}')

        # sanity: an average national resident ~ national life expectancy (meta assumption holds)
        avg = {"country": "RO", "age": 40, "sex": "M", "smoke": 0, "pa_min": 300, "sleep": 7, "waist": 98}
        _, e_avg = post("/api/estimate", avg)
        check("estimate: RO ~40yo male reaches a plausible age (72-90)",
              72 <= e_avg.get("reaches_age", 0) <= 90, f'reaches {e_avg.get("reaches_age")}')

        # input validation -> 400
        s, _ = post("/api/estimate", {**ro, "pa_min": -5})
        check("bad input -> 400", s == 400, str(s))

        # ── Bundle 7 (LEV): every answered lever reaches the number, the explanation, the overlay
        # and the advice — and an unanswered one changes nothing. ──────────────────────────────
        plain = {"country": "RO", "age": 55, "sex": "M", "smoke": 0, "pa_min": 600,
                 "sleep": 7, "waist": 95}
        _, base_est = post("/api/estimate", plain)
        base_years = base_est.get("estimate_years")

        # 1. Unanswered levers and answers at their centring reference are the same number.
        #    (The bundle's references: standardizer means for diet/sedentary/stress, level "light"
        #    for alcohol — see bundle/model-v4.0.0/coefficients.json. A bundle change makes this
        #    FAIL loudly rather than drift.) relative_risk is compared too: it is rounded to 3 dp
        #    against the years' 1 dp, so it catches a centring drift ~6x smaller.
        at_reference = {**plain, "diet_score": 2.5, "sitting_hours": 6.0,
                        "stress_score": 6.11, "alcohol": "light"}
        _, ref_est = post("/api/estimate", at_reference)
        check("levers: unanswered == answered-at-reference (national anchoring holds)",
              ref_est.get("estimate_years") == base_years
              and ref_est.get("relative_risk") == base_est.get("relative_risk"),
              f'{ref_est.get("estimate_years")}y/{ref_est.get("relative_risk")}rr vs '
              f'{base_years}y/{base_est.get("relative_risk")}rr')

        # 2. Each lever moves the estimate in the direction the evidence says.
        def years(extra):
            _, e = post("/api/estimate", {**plain, **extra})
            # A failed estimate must fail the comparison, not abort the probe.
            return e.get("estimate_years", float("nan"))

        # Query once and reuse: the number in the failure message must be the number that was
        # asserted, not a second request's answer (and each estimate persists a row).
        heavy, abstains = years({"alcohol": "heavy"}), years({"alcohol": "none"})
        check("lever: heavy drinking costs years vs abstaining", heavy < abstains,
              f"{heavy} < {abstains}")
        best_diet, worst_diet = years({"diet_score": 5}), years({"diet_score": 0})
        check("lever: a better diet outlives a worse one", best_diet > worst_diet,
              f"{best_diet} > {worst_diet}")
        check("lever: heavy sitting costs years", years({"sitting_hours": 12}) < base_years)
        check("lever: high perceived stress costs years", years({"stress_score": 16}) < base_years)
        check("context: mobility difficulty lowers the estimate", years({"mobility": 1}) < base_years)
        check("env: a polluted, grey location costs years vs a clean, green one",
              years({"pm25": 25, "ndvi": 0.3}) < years({"pm25": 8, "ndvi": 0.7}))

        # 3. The levers explain themselves (why[]) at their real evidence grade.
        risky = {**plain, "alcohol": "heavy", "diet_score": 0, "sitting_hours": 12,
                 "stress_score": 14}
        _, risky_est = post("/api/estimate", risky)
        why = {w.get("key"): w for w in risky_est.get("why", []) if isinstance(w, dict)}
        levers = ("alcohol", "diet", "sedentary", "stress")
        check("why[]: all four literature levers explain themselves",
              set(levers) <= set(why), ", ".join(sorted(why)))
        # .get() throughout: a missing lever is the regression these checks exist to catch, so it
        # must read as a FAIL, never a KeyError that aborts the remaining checks.
        check("why[]: each lever costs years and cites its evidence",
              all(why.get(k, {}).get("delta_years", 0) < 0 and why.get(k, {}).get("citation")
                  for k in levers))
        check("why[]: stress is shown at its honest (weak) grade",
              why.get("stress", {}).get("evidence") == "weak",
              str(why.get("stress", {}).get("evidence")))

        # 4. What-If prices them, and agrees with scoring them directly.
        _, wi = post("/api/whatif", {"base": risky,
                                     "changes": {"alcohol": "none", "diet_score": 5,
                                                 "sitting_hours": 4, "stress_score": 4}})
        _, improved = post("/api/estimate", {**risky, "alcohol": "none", "diet_score": 5,
                                             "sitting_hours": 4, "stress_score": 4})
        direct = round(improved.get("estimate_years", 0) - risky_est.get("estimate_years", 0), 1)
        overlay = wi.get("delta_years", 0)
        check("what-if: improving every lever adds years", overlay > 1.0, f"+{overlay}")
        # These are algebraically identical when only literature levers move; the only slack is
        # rounding (two round1'd values differenced vs one round1'd difference) => 0.15, not 0.3.
        check("what-if: the overlay agrees with scoring the change directly",
              abs(overlay - direct) <= 0.15, f"{overlay} vs {direct}")

        # 5. The advice targets them, with openable references — and never on an unanswered lever.
        _, recs = post("/api/recommendations", risky)
        by_feature = {r.get("feature"): r for r in recs
                      if isinstance(r, dict)} if isinstance(recs, list) else {}
        check("recommendations: all four levers are actionable advice",
              set(levers) <= set(by_feature), ", ".join(sorted(by_feature)))
        check("recommendations: each carries at least one openable study",
              all(by_feature.get(f, {}).get("references") for f in levers))
        # The unanswered-lever check needs a profile that DOES fire something, otherwise an empty
        # response (dead rules table, eval_condition stuck false) would pass it trivially.
        _, no_levers = post("/api/recommendations", {**plain, "smoke": 2, "waist": 105})
        fired = {r.get("feature") for r in no_levers
                 if isinstance(r, dict)} if isinstance(no_levers, list) else set()
        check("recommendations: the engine is live for this profile (non-lever rules fire)",
              bool(fired & {"smk_current", "waist"}), ", ".join(sorted(fired)))
        check("recommendations: an unanswered lever is never recommended",
              not (set(levers) & fired), ", ".join(sorted(fired)))

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
        stored = ({a.get("question_code"): a.get("value") for a in back if isinstance(a, dict)}
                  if s == 200 and isinstance(back, list) else {})
        check("interview: answers read back identically (batteries stay arrays)",
              stored.get("Q19_stress") == [3, 1, 1, 3] and stored.get("Q20_mood") == [1, 0],
              str(stored.get("Q19_stress")))
        check("interview: the location answer keeps its object shape",
              (stored.get("Q23_location") or {}).get("name") == "Cluj-Napoca")
        s, _ = post("/api/profile/location", {"name": "Cluj-Napoca", "country": "RO"}, token)
        check("interview: the home location is accepted", s == 200, str(s))
        s, profile = get_auth("/api/profile", token)
        check("interview: the home location reads back on the profile",
              bool(s == 200 and profile.get("home_location_id")), str(s))

        # Probe hygiene: erase the account we created, so runs don't accrete *accounts*. (The
        # unauthenticated estimates above still persist calculation rows to the shared anonymous
        # account — inherent to exercising the try-before-signup path.) This also witnesses the
        # GDPR erasure route and the "a token for an erased account reads as unauthenticated"
        # guard — nothing else here covers either.
        s, _ = get_auth("/api/account", token, method="DELETE")
        check("privacy: the probe's account erases itself (GDPR route)", s == 200, str(s))
        s, _ = get_auth("/api/profile", token)
        check("privacy: the erased account's token is refused (401)", s == 401, str(s))

        return finish()
    finally:
        srv.terminate()


def finish():
    ok = all(c[1] for c in checks)
    print(f"\nWITNESS: {'PASS' if ok else 'FAIL'} ({sum(c[1] for c in checks)}/{len(checks)} checks)")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
