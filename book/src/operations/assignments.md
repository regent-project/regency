# Assignments and effective expected states

An **assignment** says what a set of hosts should look like: a host
selector, one expected state, and a priority. It is what gives a host's
colour on the map a meaning: a host is green when it complies with the
expected state its assignments give it, and nothing else.

| Field | Example |
|---|---|
| hosts | a selector: `inventory infra/web`, `label env=prod`, host ids (any of them matches) |
| expected state | `infra/nginx` |
| priority | `100` (default `0`; higher = more specific) |

Like the rest of the catalog, assignments come from a Git source
(`assignments/*.yaml`, read-only in regency, see [Sources](sources.md)) or
from the API.

## Layering: one effective state per host

A host selected by several assignments gets **one effective expected
state**, built like Helm values, from the most generic layer to the most
specific:

1. its assignments are sorted by priority, lowest first;
2. their attributes are added in that order;
3. an attribute whose `Name` already came from a lower layer **replaces** it,
   in place (so a `web` assignment at priority 100 can override the
   `baseline` attribute named `swappiness`);
4. unnamed attributes are always added.

Example: `baseline` (priority 0, every host) and `web` (priority 100,
inventory `web`). A web host runs `baseline`'s attributes, with `web`'s
added or overriding them by name. A database host runs `baseline` alone.

The same attribute `Name` from two assignments of **equal** priority, with a
different content, is a conflict: regency cannot tell which one wins. The
host is flagged (*assignments in conflict*, amber on the map, both
assignments named on its panel) and **no task is built for it** until the
priorities or names are fixed.

The effective state has a revision (a hash of its content): it changes when
an assignment, its expected state or the layering changes.

## What the colours mean

| State | Meaning |
|---|---|
| compliant / not compliant | last outcome against the host's **current** effective state |
| outdated | the last outcome was against an older effective state: assess it again |
| unassigned | no assignment selects the host: nothing is checked on it (grey, never green) |
| invalid | its assignments are in conflict (see above) |

The host panel and page show the layers (assignments, lowest priority
first) and the effective state's attributes, each tagged with the assignment
it comes from.

## Assess and Reach

Assess and Reach always run each host against **its own effective state**:
from an assignment's card on the map, from an inventory or label card, from
a host, or from an order. There is no expected state to choose: running a
single layer alone could undo an override.

Orders reference an assignment, a mode and a schedule (see
[Running jobs and recurring orders](jobs-orders.md)).

## One-shot attributes

To check or fix something once, without adding it to an expected state, open
an assignment's dialog on the map (its **Assess** or **Reach** button) and
choose **Run an attribute…**. Write the attributes one of two ways, like a
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

Hosts running regops will pull their composed effective state from the
endpoint instead of their own Git repository. This needs a new regops
release (and a regent-sdk one); until then, a proactive host applies what
its own repository says.
