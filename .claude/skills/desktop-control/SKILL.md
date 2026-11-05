---
name: desktop-control
description: Operate this Windows desktop end to end -- click, type, fill browser forms, open apps, navigate sites, run multi-step tasks -- through the desktop-control MCP server. Use whenever a task requires driving the machine's actual UI (native app or browser) rather than editing files or calling an API directly. Trigger phrases include "desktop control", "control my screen", "operate my PC", "desktop agent", "click", "fill out this form", "open the app", or any request that ends at a graphical interface. Invokable directly as /desktop-control.
---

# Desktop control, quick start

You have eight tools: `desktop_windows`, `desktop_focus`, `desktop_observe`,
`desktop_act`, `desktop_verify`, `desktop_decide`, `desktop_login`,
`desktop_status`. Full reference, schemas, and troubleshooting:
[`skills/desktop-control-mcp/SKILL.md`](../../../skills/desktop-control-mcp/SKILL.md)
in the repo. Read that once before your first real task; this file is the
loop and the rules you need on every call after that.

## The loop

1. `desktop_observe()` once when you land on an unfamiliar window.
2. `desktop_act(instruction)` with one specific, plain-language step.
3. Read `verdict`. Act on it (table below).
4. `desktop_verify(expectation)` before you report a task finished.
5. Repeat from 2. **Don't call `desktop_observe` between consecutive acts**
   -- a successful `desktop_act` already returns `after` and `diff`.

## Hard rules

1. **One step per `desktop_act` call.** "Open the form, fill the name field,
   submit" is three calls, not one.
2. **Name the target by its exact on-screen label**, never by position
   ("the second one", "top right") -- that is exactly what this breaks on.
3. **Quote every literal string to type**, exactly:
   `type "jane@example.com" into the Email field`, not `enter my email`.
4. **A `blocked` result is final for that phrasing.** It means a safety
   rule fired -- rephrasing to dodge it (`delete` → `remove permanently`)
   is the same action wearing a hat, and the gate catches the paraphrase
   on the next call anyway (it re-evaluates by policy, not by whether you
   already tried once). Tell the human it was blocked and why.
5. **You cannot approve a `review` yourself.** Only a human clicking the
   native dialog that pops up on their own screen counts. If it's blocked
   because nobody answered, say so -- don't retry hoping it goes through.
6. **A `refused` result for a sensitive field is final too.** Never try to
   route around it (e.g., by using `key` presses instead of `type`). If a
   site needs login, call `desktop_login(url)` and tell the human to log in
   by hand.
7. **`desktop_verify` before claiming success.** An `executed` verdict means
   the action was dispatched, not that it had the effect you wanted --
   check `diff`, or ask Jev directly with `desktop_verify`.
8. **If `desktop_status().kill_switch_engaged` is true, stop.** A human hit
   the kill switch (`Ctrl+Alt+K`) on purpose. Don't call
   `desktop_reset_kill_switch` yourself -- ask first.

## Reading `verdict`

| verdict | what happened | what you do |
|---|---|---|
| `executed` | it happened | check `diff`/`after`, continue or verify |
| `blocked` | the safety gate stopped it (rule 4/5 above) | tell the human, stop on this phrasing |
| `escalated` | not confident, or a real error (elevated window) | read `least_certain_about`, rephrase -- never repeat verbatim |
| `halted` | kill switch engaged | stop, see rule 8 |
| `refused` | unquoted text, or a sensitive field | fix the instruction, or use `desktop_login` |

## Worked example

> **Human:** "Sign me up for the newsletter on example.com with my email
> jane@example.com."

```
desktop_act("navigate to example.com")
  -> executed, after: window "Example Domain", source "dom"

desktop_observe()
  -> elements include [id 4] DomInput "Email", [id 7] DomButton "Subscribe"

desktop_act('type "jane@example.com" into the Email field')
  -> executed, confidence 0.95

desktop_act("click the Subscribe button")
  -> executed, diff shows a new "Thanks for subscribing!" text added

desktop_verify("the page confirms the subscription was successful")
  -> holds: 0.92
```

Report it done. Four `desktop_act` calls, one `desktop_observe`, one
`desktop_verify` -- not a dozen coordinate guesses.
