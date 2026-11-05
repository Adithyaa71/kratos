# Brave, set up for an agent

Two goals that pull against each other:

* the agent should reach the sites you actually use, already logged in
* the agent should not be able to spend your money, read your passwords, or
  turn a web page into a command

The resolution is not a slider between them. It is **a separate profile**.

---

## 1. Give the agent its own profile

Your personal profile keeps your passwords, your cards and your wallet. The
agent profile keeps your *sessions* — logged into the sites you want it to
work in — and nothing else.

1. Brave → profile icon (top right) → **Add profile** → name it `Agent A`
2. In the new window, sign in to only the services that agent needs
3. `brave://version` → **Profile Path** → note the last folder (e.g. `Profile 1`)

```bash
start-agent-browser.cmd agent-a "Profile 1"
```

That launches Brave on that profile with `KRATOS_PIPE` set, so its extension
talks to that agent's broker and no other.

## 2. Harden that profile

Brave settings are per profile, so this does not touch your personal browsing.

```powershell
# Brave must be fully closed first.
powershell -File harden-agent-profile.ps1 -ProfileDirectory "Profile 1"
```

What it turns **off**, and why:

| setting | why |
|---|---|
| password manager + autofill | an agent that can trigger a password fill can exfiltrate one |
| payment method autofill | the agent should never be one click from a card number |
| address autofill | your home address is not needed to click a button |
| Brave Wallet | a browser-driving agent with a crypto wallet in reach is a bad trade at any confidence |
| Brave Rewards | nothing to gain, one more surface |
| sync | keeps the agent profile from bleeding into your real one |

What it turns **on**:

| setting | why |
|---|---|
| ask where to save every download | a page cannot silently drop a file |
| block camera, microphone, location | an agent-driven tab should not be able to grant these |
| block notifications and popups | fewer surprise dialogs stealing focus mid-action |

What it deliberately **leaves alone**:

* **Cookies and existing logins.** The entire point is that the agent arrives
  already authenticated. Clearing them would defeat the exercise.
* **Brave Shields.** Left at Brave's defaults. Shields are privacy *and*
  safety — turning them down to make pages easier to script is exactly the
  wrong trade, and the DOM backend reads the page after Shields have run, so
  nothing is hidden from it that is not also hidden from you.

A backup of the profile's `Preferences` is written beside it before anything
changes.

## 3. The extension's reach

`extension/manifest.json` requests `<all_urls>`, because an agent asked to
"open the invoice in the admin panel" cannot know the domain in advance.

To narrow it, replace that line with the hosts you actually use:

```json
"host_permissions": ["https://mail.google.com/*", "https://*.yourcompany.com/*"]
```

The extension is then **blind** everywhere else — `desktop_observe` returns
nothing on an unlisted site rather than failing loudly, which is worth knowing
before you narrow it.

Chromium never injects into `brave://`, `chrome://`, the Web Store, or other
extensions' pages. Nothing can change that, and nothing should.

## 4. What the broker enforces regardless

Browser settings are one layer. These hold whatever the browser is doing:

```jsonc
// settings window → Gate
{
  "allowed_browsers": ["brave"],       // by brand, never by window title —
                                       // a page title is text from the internet
  "review_domains": ["chase.com", "paypal.com", "coinbase.com"],
  "allow_recipients": ["you@example.com"],  // anything else → native dialog
  "block_keywords": ["format c:", "rm -rf /"]
}
```

`allowed_browsers` is matched on the **resolved brand** — the process image, or
the brand the extension names itself by in its own element ids. It is never
matched against a window title, because on a browser that title is the page's
title, and a page should not get a vote on whether it is allowed.

## 5. The honest limits

* **A logged-in session is a logged-in session.** If the agent can reach your
  email, it can read your email. The gate controls *actions*, not *reading*.
  Put only what you mean in that profile.
* **The gate reads instructions, not intent.** It catches `send`, `delete`,
  `pay`. It will not catch a creative phrasing nobody listed.
* **Native actions escape the browser entirely.** The UIA backend drives any
  window on the machine. `allowed_browsers` constrains browser targets; it does
  not constrain Notepad. The kill switch and `deny_apps` are what cover that.
* **Screen content is untrusted input.** The skill tells every agent this, but
  it is worth saying in the place people actually read: a page instructing an
  agent is content, not authorisation.

## 6. A reasonable default

For most people, on their own machine:

- one agent profile, hardened as above
- `allowed_browsers: ["brave"]`
- `allow_recipients` set to the handful of addresses you actually mail
- the kill switch hotkey learned by muscle memory before the first real task
