# Templates: ready-made pages with a short form

A **template** describes a kind of page someone might want, such as "usage limits" or "Pi status", as a small JSON file. It has a short **settings form** (the questions to ask, such as "Which service?") and a list of the **pages** the answers make. The desktop manager shows the form, checks the answers and adds the pages: in **Pages**, choose **Add from template**. For now the manager offers only its built-in templates; loading one from a file comes later. [`starters/helper/template.json`](../starters/helper/template.json) is a small example.

A template holds no code. It is only fields, options, simple conditions and `{key}` placeholders, so a template from anywhere can't run anything on your computer or the hub.

Each page made from a template remembers the template's id and the answers (its **settings**). The page's helper can read them back, so one helper program can serve pages set up in different ways (see [helpers.md](helpers.md#reading-the-pages-settings)).

**Settings are not secret.** Every helper invited for a page's feed can read them, and they show in the manager. Never ask for API keys, passwords or tokens in a template. A helper that needs one keeps it on its own computer.

## The templates that come with Display Hub

They are in `crates/templates/templates/`:

| Id | Makes |
|---|---|
| `usage-limits/1` | A usage-limit meter for Claude Max, ChatGPT Plus or OpenCode Go Plus, exactly as the manager's "Add page" does today. "Helper" is offered only for Claude Max and ChatGPT Plus. |
| `pi-status/1` | A big number page for the `pi-status` helper on the hub's Pi, with the one command that starts that helper. |
| `big-number/1`, `short-text/1`, `list/1`, `meter/1` | One typed-in page of each ready-made shape. |

## An example

This is a complete template a maker could start from. It asks which bed, an optional name, a sensor depth, what to show and whether to show a forecast, and makes up to three pages:

```json
{
  "id": "garden/1",
  "name": "Garden",
  "description": "Soil moisture and rain for one garden bed, sent by the garden helper.",
  "setup": {
    "help": "Run this once on the hub's Raspberry Pi to start the helper that fills these pages.",
    "command": "sudo display-hub-helper add garden --run /usr/local/lib/garden-helper --feed garden-north --feed garden-south"
  },
  "settings": [
    { "key": "bed", "label": "Bed", "type": "choice", "required": true,
      "options": [
        { "value": "north", "label": "North bed" },
        { "value": "south", "label": "South bed" }
      ] },
    { "key": "name", "label": "Name", "help": "Leave blank to use the bed's name.",
      "type": "text", "max_length": 20 },
    { "key": "depth", "label": "Sensor depth (cm)", "type": "number", "min": 1, "max": 60, "default": 10 },
    { "key": "shows", "label": "Shows", "type": "multi_choice", "min": 1, "default": ["moisture"],
      "options": [
        { "value": "moisture", "label": "Moisture" },
        { "value": "rain", "label": "Rain", "when": { "bed": "south" } }
      ] },
    { "key": "forecast", "label": "Show the forecast", "type": "yes_no" }
  ],
  "pages": [
    { "title": "{name|bed.label}", "kind": "meter", "source": "helper", "feed": "garden-{bed}",
      "when": { "shows": "moisture" } },
    { "title": "Rain, {bed.label}", "kind": "big_number", "source": "helper", "feed": "garden-{bed}",
      "when": { "shows": "rain" } },
    { "title": "Forecast", "kind": "text", "source": "manual", "when": { "forecast": true } }
  ]
}
```

Answering `{"bed": "north"}` makes one meter page called "North bed" on the feed `garden-north`, with the settings `{"bed": "north", "depth": 10, "shows": ["moisture"], "forecast": false}`.

## The format

Unknown fields are refused, so a typing mistake is caught rather than ignored. A template file is at most 16 KiB.

### The template

| Field | Required | What it is |
|---|---|---|
| `id` | yes | A name and a version, written like a custom kind: lower-case letters, digits and hyphens, then `/` and a number from 1, such as `usage-limits/1`. Give a changed template a new version, so pages made from the old one can still be told apart. |
| `name` | yes | What the manager calls it, up to 48 characters. |
| `description` | yes | One or two plain sentences saying what the pages show, up to 280 characters. |
| `setup` | no | One step the owner takes once by hand, such as starting a helper: `help` (where and why, up to 280 characters) and `command` (exactly what to type, up to 500 characters). The manager will show the command with a Copy button. |
| `settings` | no | The form: up to 20 settings, in the order shown. |
| `pages` | yes | The pages the answers make: 1 to 10, in order. |

All text is plain: no line breaks, and no spaces at the start or end.

### Settings

| Field | For | What it is |
|---|---|---|
| `key` | all | The answer's name in the page's settings: a lower-case letter, then lower-case letters, digits and underscores, up to 32 characters, such as `show_weekly`. Each key once. |
| `label` | all | The question, up to 48 characters. |
| `help` | all | A sentence under it, up to 280 characters. Optional. |
| `type` | all | `text`, `number`, `choice`, `multi_choice` or `yes_no`. |
| `required` | all | `true` if it must be answered. Optional; a yes/no setting always has an answer (no, unless its default says yes). |
| `default` | all | The answer when none is given. It must be a valid answer itself. |
| `max_length` | `text` | The most characters, 1 to 500. Without it, 100. |
| `min`, `max` | `number` | The lowest and highest numbers allowed. |
| `min`, `max` | `multi_choice` | How few and how many options may be chosen. |
| `options` | `choice`, `multi_choice` | 1 to 20 options: `value` (what is stored, a lower-case slug like `claude-max`), `label` (what the owner sees) and, optionally, `when` (offer it only when other answers match; see below). |

How answers are stored:

| Type | Stored as |
|---|---|
| `text` | A string. Line breaks and runs of spaces become one space, and the ends are trimmed. A blank answer counts as not answered, so the default (if any) is used. Too long is refused, never cut. |
| `number` | A number. Whole numbers are stored without a decimal point. |
| `choice` | The chosen option's `value`. |
| `multi_choice` | A list of the chosen options' values, each once. |
| `yes_no` | `true` or `false`. |

Optional settings that weren't answered and have no default are left out. All the settings of a page together must fit in 2048 bytes as JSON, nested at most three levels deep (the hub refuses more). The form checks this too, and says so in plain words.

### Pages

| Field | Required | What it is |
|---|---|---|
| `title` | yes | The page's title. May hold placeholders. It is tidied and cut to 48 characters, like any title. |
| `kind` | yes | A ready-made shape (`meter`, `big_number`, `text`, `list`) or a custom `name/version` kind. |
| `source` | yes | `manual` (typed in), `helper`, `demo` or `none`; or `{key}`, a single placeholder for a required choice (or one with a default) whose options are all sources. |
| `feed` | for helpers | The feed that fills the page. May hold placeholders; the result must be a feed name (lower-case words joined by single hyphens). A page that can take content from a helper needs one. |
| `when` | no | Make this page only when the answers match. |

### Placeholders

In a page's `title` and `feed`:

- `{key}` is the answer to that setting: the text, the number, the option's value, a multi-choice's values joined by ", ", or "yes" or "no".
- `{key.label}` is the label of the option chosen, for a choice or multi-choice.
- `{a|b}` is `a`'s answer, or `b`'s when `a` is blank. Any number may be chained, such as `{title|service.label}`.

Braces are only for placeholders. Nothing else is filled in.

### Conditions

A `when` (on an option or a page) names one or more settings, each with a value or a list of values:

```json
"when": { "service": ["claude-max", "chatgpt-plus"], "show_weekly": true }
```

It matches when every setting named has one of the values listed. A multi-choice setting matches when any of its chosen options is listed. Only choice, multi-choice and yes/no settings can be tested, and only with values they can have. An option can't depend on its own setting.

When an answer picks an option whose condition doesn't match, the form refuses it with a sentence such as "Helper isn't available when Service is OpenCode Go Plus." If the answers make no pages at all, the form says so.

## In code

The `hub-templates` crate (`crates/templates`) reads and checks templates and fills them in:

- `Template::parse` reads a template's JSON and checks the template itself (`Template::check`), with a message naming the setting or page that is wrong.
- `Template::settings_from` checks a filled-in form and gives the settings to record.
- `Template::make_pages` does that, then gives the `NewPage` bodies to send to `POST /v1/admin/pages`, each with the template's id and the settings.
- `Field::available_options` gives the options to offer, given the answers so far.
- `built_in()` and `built_in_template(id)` give the templates above.

The hub never reads templates. It keeps each page's `template` and `settings` as given, after checking the template id's spelling and the settings' size ([architecture.md](architecture.md#admin-api)).
