# First steps in the web UI

Everything below can also come from Git (see
[Sources](../operations/sources.md)); here it is done in the web UI. Items
created in the UI belong to an API/frontend source (`ui`, created when
needed).

## 1. Add a host

On the **Hosts** page, **Add host** offers two ways, one tab each.

### Over SSH, from the runners (nothing to install on the host)

At its first start the runner generated its own SSH key. The **Over SSH**
tab shows its
public key with a ready command; run it on the host, as the account the
runner will log in as:

```bash
mkdir -p ~/.ssh && chmod 700 ~/.ssh && cat >> ~/.ssh/authorized_keys <<'EOF'
ssh-ed25519 AAAA… regency-runner-default
EOF
chmod 600 ~/.ssh/authorized_keys
```

Then fill in the form below it: host id (`web-01`), address (`10.0.0.5`,
port 22 unless `host:port`), user, and optionally labels (`role=web`: they
decide which assignments the host gets) and
the host key fingerprints (required from security level 2:
`ssh-keyscan 10.0.0.5 | ssh-keygen -lf -`). **Add the host** creates its
definition (in source `ui`), with the runner's key as its credentials.

**Administration → Host definitions** edits it later, or defines hosts with
other credentials (regent-sdk JSON, secrets as references only).

### With the regops agent

Install [regops](https://github.com/regent-project/regops) on the host,
then in the **With regops** tab give the host id (regops' `host_id`, by default its
hostname) and click **Issue a token**. The dialog shows the commands writing
the endpoint's CA certificate and the token on the host, and the lines to
put in `/etc/regops/config.toml`; restart regops. The host appears after its
first pass. Its expected state still comes from its own Git repository (see
[Proactive hosts](../operations/proactive-hosts.md)).

The endpoint address given to agents is the web UI's host with the
endpoint's port (8443); set another one in **Settings → Endpoint URL** (for
example behind a proxy or a DNS name).

## 2. Add an expected state and assign it

1. **States → New expected state**: a name
   (`base`) and its attributes in regent-sdk YAML, checked as you type:

   ```yaml
   - Name: motd
     Privilege: !None
     Detail: !LineInFile
       FilePath: /etc/motd
       State: !Present
         Create: true
       Line: !Raw "Managed by regency"
   ```

2. **Assignments → New assignment**: a name (`base`), the hosts as a label
   selector (`{role="web"}`, or `{}` for every host; the form shows the
   hosts it selects as you type) and the expected state `ui/base`, picked
   from the catalog (so it is created first). More states can be stacked on
   it, each overriding the previous ones; the **Final expected state** next
   to the form shows the result.

A host gets every assignment whose selector matches its labels: label a
host `role=web` and it gets the assignments selecting `role="web"`. A host
no assignment selects is *unassigned*: nothing runs on it.

## 3. Assess

On the assignment's row, or on the host's page (`/hosts/web-01`), click
**Assess**. The job appears in **Jobs** (`PENDING` → `STARTED` →
`FINISHED`), the host appears on the **Live view** with its colour, and its
page shows the remediations that would be needed and the outcome history.

**Reach** applies them. Both are recorded; nothing is lost if a service
restarts meanwhile.

## 4. Look inside

- **Administration → Services**: every running instance, its version, health
  and effective configuration (where each value comes from).
- **Administration → Security**: the security level of every service.
- RabbitMQ's management UI and CouchDB's Fauxton are not exposed by the
  quick start; add their ports to the compose file to explore the queues
  and documents.
