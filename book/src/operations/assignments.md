# Assignments and effective expected states

An **assignment** says what some hosts should look like: a **label
selector** (the hosts it applies to, in the style of Loki or Kubernetes),
a **priority** and a **stack of expected states**, like Helm values files
(`-f values.yaml -f prod.yaml`): the first state is the base, and each next
one overrides the previous ones. The result is the assignment's **final
expected state**.

| Field | Example |
|---|---|
| hosts | `{role=~"web\|api", env="prod"}` (`{}` = every host) |
| priority | `10` (order among a host's assignments, lower first; default 0) |
| expected states | `infra/nginx`, then `infra/nginx-prod` |

A host gets **every** assignment whose selector matches its labels, like a
Salt top file targeting grains or Ansible groups: adding the label
`monitoring="true"` to a host gives it the `monitoring` assignment, nothing
else to edit. Their final states add up into the host's **effective
expected state**. A host has a state **per assignment** (did it comply
with that assignment's state when last checked?), and its colour on the
map is the worst of them: a host is green when it complies with each of
its assignments, and nothing else.

Like the rest of the catalog, assignments come from a Git source
(`assignments/*.yaml`, read-only in regency, see [Sources](sources.md)),
the API, or the web UI: **Assignments** creates, edits and deletes them.
The expected states are picked from the catalog, so they are created first
(**States**). The form reads like a sheet: source, name and priority on
top, then two columns, **Hosts** and **Expected states**, whose rows line
up. The hosts are chosen like Grafana's label filters on Loki
(**Builder**: each filter is a chip, `env="prod"`, on one line; **+ Label
filter** or a click on a chip edits one with a label, an operator and a
value picked from the labels in use), or written as text (**Code**). The
stack is a line of chips too, base first, each moved with ◀ ▶ or removed
with ×. Below them, **Matching hosts** lists the hosts the selector
selects, and **Final expected state** shows the result as you edit: the
suspected conflicts and every attribute with its origin. Clicking an
assignment's name shows the result read-only.

![Building an assignment in the web UI: label filters kind=~"web|api" then env="prod" narrow the matching hosts from 2,479 to 658 live; the expected states baseline, web and prod-tuning are stacked (the last wins) and the final expected state shows each attribute's origin, swappiness from prod-tuning overriding baseline](../images/assignment.gif)

*Building an assignment: two label filters select the production web and
API hosts (658 of 2,479), and the stack `baseline`, `web`, `prod-tuning`
gives a final state where `prod-tuning`'s `swappiness` overrides the
baseline's.*

## Selectors

| Matcher | Holds when |
|---|---|
| `key="value"` | the label equals `value` |
| `key!="value"` | it does not (a missing label reads as `""`) |
| `key=~"re"` | it matches the regular expression (anchored: the whole value) |
| `key!~"re"` | it does not |

All the matchers must hold. Every host also has the built-in labels
`host` (its id) and `source` (the sources defining it, `regops` for a
reporting agent): `{host="web-01"}` is one host.

Assignments can also be created, edited and connected to hosts on the
map, by relabelling hosts or changing a selector: see
[Editing on the map](hosts.md#editing-on-the-map).

## Overrides inside a stack: the last state wins

1. The states are taken in order, base first.
2. Their attributes are added in that order.
3. An attribute whose `Name` already came from an earlier state
   **replaces** it, in place: `nginx-prod` can override `nginx`'s attribute
   named `nginx workers`.
4. Unnamed attributes are always added.

Each attribute of the final state shows the state it comes from and, if
it replaced attributes of the same `Name`, the states it overrides (like
`git blame`): `nginx workers` · `infra/nginx-prod` · *overrides
infra/nginx*.

## Several assignments on a host: they add up

A host's assignments are applied by **priority** (lower first, like the
`10-`, `20-` prefixes of a `conf.d` directory), then by name. Their final
states are added in that order, and each attribute shows the assignment it
comes from. An override is intended inside one stack, never between
assignments, so:

- an attribute **identical** to one of an earlier assignment (same content,
  whatever its `Name`, so a typo'd name is still caught) is applied once,
  marked *also in* the later assignments;
- the same `Name` with another content, or the same target with another
  content (service `httpd` *Started* in one assignment, *Stopped* in
  another; two different `LineInFile` on the same line of the same file) is
  a **conflict**: the host is *invalid* (amber on the map, the conflict
  named on its panel) and **no task is built for it** until an assignment
  or a state is fixed. To override on purpose, put both states in one
  stack.

"Baseline everywhere, nginx on the web hosts" is two assignments:
`baseline` on `{}` and `web` on `{role="web"}`. A web host that should not
get a baseline attribute is the case for one stack: `[baseline, web]` on
the web hosts, and `baseline` on `{role!="web"}`.

Two assignments with the **same priority and the same name** (from two
sources) cannot be ordered: saving one through the web UI or the API is
refused until the priority or the name of one of them is changed, a Git
source reports it as an error on the file, and a host they would both
select is *invalid*.

Each assignment has a revision (a hash of the assignment, its stack and its
states' revisions): it changes whenever one of them changes.

## Suspected conflicts

Inside one stack, regency points out what looks like a mistake, without
refusing it (saving and running stay allowed; the ⚠ marks the attributes
involved; between assignments the same cases are conflicts, see above):

- two attributes with different `Name`s acting on the same thing
  differently: service `httpd` *Started* in one, *Stopped* in another; a
  package present and absent; two `LineInFile` on the same line of the same
  file; two `Hostname`;
- the same attribute twice, under two names;
- an override changing an attribute's nature (a `Service` replaced by a
  `LineInFile` of the same `Name`).

To silence one, give both attributes the same `Name` (the later one then
overrides the earlier one), or remove one.

## What the colours mean

| State | Meaning |
|---|---|
| compliant / not compliant | the host's last check of an assignment, at its **current** revision |
| outdated | the last check was against an older version of the assignment: assess it again |
| unassigned | no assignment selects the host: nothing is checked on it (grey, never green) |
| invalid | conflicting or ambiguous assignments, or a state of a stack is missing (see above) |

The table applies to each assignment of a host; the host's own colour is
the worst of them (a host never checked against one of its assignments
stays grey). The host panel, page and hover tip list its state per
assignment; the host page also shows its assignments in order, each with
its stack, and the effective state's attributes, each tagged with its
state and assignment.

## Assess and Reach

Assess and Reach of an **assignment** (its card on the map, its row in
**Assignments**, an order) run that assignment's final state only, on the
hosts it selects: "reach this state, because these hosts carry these
labels". The hosts' other assignments are not touched, and their colours
for them do not change. This is safe because assignments never override
each other: an attribute set differently by two assignments is a conflict,
and a host with conflicting assignments gets no job at all.

Assess and Reach of a **host** (its page) run each of its assignments, one
job each.

Orders reference an assignment, a mode and a schedule (see
[Running jobs and recurring orders](jobs-orders.md)).

## One-shot attributes

To check or fix something once, without adding it to an expected state,
click **Run** on an assignment's card on the map. Write the attributes one of two ways, like a
query in Grafana, and switch at any time:

- **Builder**: a form. Pick the attribute's **nature** (LineInFile, Apt,
  Service, Iptables, … every one regent-sdk has, grouped by category as in
  the SDK: ai, container, gpu, network, observability, package, shell,
  system, utilities), then fill in its fields; choices
  are lists, optional fields are added with their **+** button, and each
  field explains itself. The form comes from regent-sdk's own description of
  its attributes, so it always matches what the runners accept, and the
  attributes are checked as you type ("✓ valid attributes" or the problem).
- **Code**: the YAML (one attribute, a list, or a whole expected state).

The dialog starts empty: nothing is pre-filled, neither an example nor the
assignment's expected state. In the builder, picking a type or a choice
only adds its fields, blank, and lists what is still to fill in until the
attributes are valid; Run is enabled then.

Switching converts between the two (YAML the form cannot hold is reported
and stays in Code; an unfinished form switches to Code without changing
its text). Then pick Assess or Reach and run it on the
assignment's hosts. The dialog follows the jobs live, with each host's
result and outcome.

One-shot outcomes are kept in the hosts' history (their origin is the
command id) but **never change a host's colour**. Inline credentials are
refused, as everywhere. The browser remembers the last 20 runs, to show
their results again.

## Proactive hosts

A host running regops joins assignments by the labels its agent reports,
but still applies the state of its own Git repository; regency warns when
that state differs from its effective state (see [Proactive
hosts](proactive-hosts.md#in-the-fleet-labels-assignments-freshness)).
Pulling the final state from regency needs a new regops release.
