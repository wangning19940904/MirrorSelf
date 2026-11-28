"""Generates the synthetic Feishu calendar fixture in this directory.

Every person, room, id, link and meeting is invented. The output mimics what
`lark-cli` 1.0.x prints (field names, value types, envelope). Run from the
repository root:

    python3 fixtures/feishu/generate.py fixtures/feishu

and update crates/core/tests/feishu_calendar.rs with any change.
"""

import json, sys
from datetime import datetime, timezone, timedelta

OUT = sys.argv[1]
CST = timezone(timedelta(hours=8))
JST = timezone(timedelta(hours=9))

def ts(y, mo, d, h, mi, tz=CST):
    return str(int(datetime(y, mo, d, h, mi, tzinfo=tz).timestamp()))

USERS = {
    "Demo User": "ou_0000000000000000000000000000d001",
    "Lin Qiao": "ou_0000000000000000000000000000a001",
    "Mara Okafor": "ou_0000000000000000000000000000a002",
    "Avery Chen": "ou_0000000000000000000000000000a003",
    "Jordan Li": "ou_0000000000000000000000000000a004",
    "Sam Ito": "ou_0000000000000000000000000000a005",
    "Riley Novak": "ou_0000000000000000000000000000a006",
    "Quinn Zhao": "ou_0000000000000000000000000000a007",
    "Haru Sato": "ou_0000000000000000000000000000a008",
    "Former Member": "ou_0000000000000000000000000000a009",
}
ROOMS = {
    "F3-Orion (8)": "omm_000000000000000000000000000r0001",
    "F5-Vega (12)": "omm_000000000000000000000000000r0002",
    "F2-Lyra (4)": "omm_000000000000000000000000000r0003",
}
CAL = "feishu.cn_demo000000000000000000@group.calendar.feishu.cn"

def org(name):
    return {"display_name": name, "user_id": USERS[name]}

def room(name, status):
    return {"attendee_id": "resource_" + ROOMS[name][4:], "display_name": name, "is_organizer": False,
            "room_id": ROOMS[name], "rsvp_status": status, "type": "resource"}

def user(name, status, organizer=False, optional=None):
    a = {"attendee_id": "user_" + USERS[name][3:], "display_name": name, "is_organizer": organizer,
         "rsvp_status": status, "type": "user", "user_id": USERS[name]}
    if optional is not None:
        a["is_optional"] = optional
    return a

def chat(name, status):
    return {"attendee_id": "chat_0000000000000000000000000c0001", "chat_id": "oc_000000000000000000000000000c0001",
            "display_name": name, "rsvp_status": status, "type": "chat"}

def third(name, email, status):
    return {"attendee_id": "third_party_0000000000000000000000t0001", "display_name": name,
            "is_external": True, "rsvp_status": status, "third_party_email": email, "type": "third_party"}

def uid(n):
    return f"5e1a0c00-0000-4000-8000-{n:012x}"

def instance(n, summary, start, end, organizer, *, rsvp="accept", status="confirmed", rooms=(),
             tz="Asia/Shanghai", recurring=None, exception=False, vc="vc", location=None, all_day=False):
    eid = f"{uid(recurring or n)}_{start if recurring else 0}"
    ev = {
        "app_link": f"https://applink.feishu.cn/client/calendar/event/detail?calendarId=0000000000000000001&key={uid(recurring or n)}&originalTime={start if recurring else 0}&startTime={0 if all_day else start}",
        "attendee_ability": "can_modify_event" if organizer == "Demo User" else "can_invite_others",
        "attendees": list(rooms),
        "color": -1,
        "description": "",
        "end_time": {"date": end, "timezone": "UTC"} if all_day else {"timestamp": end, "timezone": tz},
        "event_id": eid,
        "event_organizer": org(organizer),
        "free_busy_status": "free" if all_day else "busy",
        "is_exception": exception,
        "organizer_calendar_id": CAL,
        "self_rsvp_status": rsvp,
        "start_time": {"date": start, "timezone": "UTC"} if all_day else {"timestamp": start, "timezone": tz},
        "status": status,
        "summary": summary,
        "vchat": {"vc_type": vc} if vc in ("no_meeting",) else {"meeting_url": "https://vc.feishu.cn/j/000000" + f"{n:03d}", "vc_type": vc},
        "visibility": "default",
    }
    if recurring:
        ev["recurring_event_id"] = f"{uid(recurring)}_0"
    if location:
        ev["location"] = location
    if not ev["attendees"]:
        del ev["attendees"]
    ev["description_rich"] = ""
    return ev

def get(ev, attendees, create_time):
    e = {k: v for k, v in ev.items() if k not in ("attendees", "description_rich")}
    e["attendees"] = attendees
    e["create_time"] = create_time
    e["has_more_attendee"] = False
    e["is_external"] = False
    e["recurrence"] = ""
    e["reminders"] = [{"minutes": 5}]
    return {"ok": True, "identity": "user", "data": {"event": dict(sorted(e.items()))}}

def envelope(items, **extra):
    data = {"items": items}
    data.update(extra)
    return {"ok": True, "identity": "user", "data": data}

# --- events -----------------------------------------------------------------
E1 = instance(1, "Weekly platform sync", ts(2026,10,12,10,0), ts(2026,10,12,11,0), "Lin Qiao",
              rooms=[room("F3-Orion (8)", "accept")], recurring=1)
E2 = instance(2, "Design review: onboarding flow", ts(2026,10,12,10,30), ts(2026,10,12,11,30), "Mara Okafor",
              rooms=[room("F5-Vega (12)", "accept"), room("F2-Lyra (4)", "decline")])
E3 = instance(3, "1:1 Avery / Demo", ts(2026,10,13,14,0), ts(2026,10,13,14,30), "Avery Chen", rsvp="needs_action")
E4 = instance(4, "Release window v2.14", ts(2026,10,13,23,0), ts(2026,10,14,1,0), "Demo User")
E5 = instance(5, "Team offsite", "2026-10-14", "2026-10-14", "Jordan Li", all_day=True, vc="no_meeting")
E6 = instance(6, "Vendor demo", ts(2026,10,14,9,0), ts(2026,10,14,9,45), "Sam Ito", rsvp="decline")
E7 = instance(7, "Budget planning", ts(2026,10,14,15,0), ts(2026,10,14,16,0), "Riley Novak", status="cancelled")
E1b = instance(1, "Weekly platform sync", ts(2026,10,15,10,0), ts(2026,10,15,11,0), "Lin Qiao",
               rooms=[room("F3-Orion (8)", "accept")], recurring=1)
E8 = instance(8, "Partner sync (US)", ts(2026,10,15,8,0), ts(2026,10,15,9,0), "Quinn Zhao",
              tz="America/Los_Angeles", vc="third_party")
E9 = instance(9, "Architecture review", ts(2026,10,15,14,0), ts(2026,10,15,15,0), "Demo User",
              rooms=[room("F3-Orion (8)", "accept")])
E10 = instance(10, "Tokyo office hours", ts(2026,10,16,10,0,JST), ts(2026,10,16,11,0,JST), "Haru Sato",
               tz="Asia/Tokyo", vc="no_meeting",
               location={"address": "1-2-3 Example-cho, Chiyoda-ku", "latitude": 35.68, "longitude": 139.76,
                         "name": "Tokyo office, 12F"})
# Second fetch: E1b declined, E9 moved and renamed, new E11 and E12.
E1b_v2 = dict(E1b, self_rsvp_status="decline")
E9_v2 = instance(9, "Architecture review (moved)", ts(2026,10,15,16,0), ts(2026,10,15,17,30), "Demo User",
                 rooms=[room("F3-Orion (8)", "accept")])
E11 = instance(11, "Sprint planning", ts(2026,10,16,13,0), ts(2026,10,16,14,30), "Jordan Li",
               rooms=[room("F5-Vega (12)", "accept")])
E12 = instance(12, "Interview panel", ts(2026,10,16,13,30), ts(2026,10,16,14,0), "Riley Novak", rsvp="tentative")


fetch1 = [
    envelope([E2, E1, E3, E4, E6, E5, E7, E8, E1b, E9, E10]),
    get(E1, [user("Lin Qiao", "accept", True), user("Demo User", "accept"), user("Jordan Li", "accept", optional=True)] + E1.get("attendees", []), "1759000000"),
    get(E2, [user("Mara Okafor", "accept", True), user("Demo User", "accept"), user("Avery Chen", "tentative")] + E2.get("attendees", []), "1759100000"),
    get(E9, [user("Demo User", "accept", True), user("Lin Qiao", "accept"), user("Mara Okafor", "tentative"),
             chat("Platform team", "needs_action"), third("Partner Contact", "partner.contact@example.com", "needs_action")] + E9.get("attendees", []), "1759200000"),
]
fetch2 = [
    envelope([E8, E1b_v2, E9_v2], has_more=True, page_token="synthetic_page_token_2"),
    envelope([E10, E11, E12], has_more=False),
    get(E11, [user("Jordan Li", "accept", True), user("Demo User", "accept"), user("Avery Chen", "accept"),
              user("Sam Ito", "decline"), user("Former Member", "removed")] + E11.get("attendees", []), "1759300000"),
]

def dump(values, path):
    with open(path, "w") as f:
        for v in values:
            f.write(json.dumps(v, indent=2, ensure_ascii=False))
            f.write("\n")

dump(fetch1, f"{OUT}/fetch-1_2026-10-12_2026-10-17.json")
dump(fetch2, f"{OUT}/fetch-2_2026-10-15_2026-10-20.json")
