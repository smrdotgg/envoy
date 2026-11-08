# HTML email fixtures: sources and licenses

These files exercise `envoy_core::html` (see `crates/envoy-core/tests/html_fixtures.rs`). None
is real mail: they are open-source email templates, or written for Envoy. Remote image URLs are
left as they are; tests never fetch them.

## Third-party templates (MIT)

Each project's MIT license allows copying and modifying with its notice kept; the notices
are reproduced below.

| Fixture | Source | Changes |
|---|---|---|
| `cerberus-hybrid.html` | [Cerberus](https://github.com/emailmonday/Cerberus) `cerberus-hybrid.html` (master, fetched 2026-11-08) | none |
| `cerberus-responsive.html` | Cerberus `cerberus-responsive.html` | none |
| `cerberus-fluid.html` | Cerberus `cerberus-fluid.html` | none |
| `munroe-responsive.html` | [leemunroe/responsive-html-email-template](https://github.com/leemunroe/responsive-html-email-template) `email.html` (the un-inlined version: styles in `<style>`) | none |
| `postmark-receipt.html` | [ActiveCampaign/postmark-templates](https://github.com/ActiveCampaign/postmark-templates) `templates/basic/receipt/content.html` (un-inlined, class-based) | Mustachio placeholders filled with sample values; `{{#each}}` expanded to three rows |
| `postmark-invoice.html` | postmark-templates `templates/basic/invoice/content.html` | as above |
| `postmark-invoice-inlined.html` | postmark-templates `templates-inlined/basic/invoice/content.html` (the same invoice with styles inlined) | as above |
| `postmark-welcome.html` | postmark-templates `templates/basic/welcome/content.html` | placeholders filled |
| `postmark-password-reset.html` | postmark-templates `templates/basic/password-reset/content.html` | placeholders filled |
| `postmark-comment.html` | postmark-templates `templates/basic/comment-notification/content.html` | placeholders filled; attachment block removed |
| `foundation-hero.html` | [Foundation for Emails](https://github.com/foundation/foundation-emails) `templates/hero.html` and `dist/foundation-emails.css` (develop) | Inky markup compiled by hand to the tables Inky 1.x emits; the full stylesheet inlined in `<style>`, as the starter project's page does before inlining |

### Cerberus

```
The MIT License (MIT)

Copyright (c) 2017 Ted Goas

Permission is hereby granted, free of charge, to any person obtaining a copy of
this software and associated documentation files (the "Software"), to deal in
the Software without restriction, including without limitation the rights to
use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of
the Software, and to permit persons to whom the Software is furnished to do so,
subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS
FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR
COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER
IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN
CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
```

### responsive-html-email-template

```
The MIT License (MIT)

Copyright (c) [2013] [Lee Munroe]

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

### postmark-templates

```
The MIT License (MIT)

Copyright (c) 2015 Wildbit

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

### Foundation for Emails

```
Copyright (c) 2017 Foundation, inc.

MIT License

Permission is hereby granted, free of charge, to any person obtaining a copy of this software and associated documentation files (the "Software"), to deal in the Software without restriction, including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
```

Not used: Mailchimp's email-blueprints (CC BY-SA 3.0, a share-alike license that doesn't fit
an MIT repository) and mjmlio/email-templates (no license). `mjml-newsletter.html` instead
follows the markup MJML 4 emits, written for Envoy.

## Written for Envoy (MIT, as the rest of the repository)

People, companies, addresses and numbers in these are made up; domains are `example.*`.

| Fixture | What it stands for |
|---|---|
| `plain-personal.html` | A personal message composed in Gmail: divs and `<br>`, a list, a link, a signature |
| `gmail-reply.html` | A Gmail reply with two levels of `gmail_quote` blockquotes |
| `outlook-reply.html` | Outlook desktop (Word HTML): `MsoNormal` classes in `<style>`, `o:p`, pt units, a bordered table with per-side borders, the "From:" header block |
| `receipt-inline.html` | An inline-styled payment receipt: preheader, three-column summary, line items with `nowrap` amounts, a `colspan` divider, a `bgcolor` button cell, a tracking pixel |
| `mjml-newsletter.html` | MJML 4 output: `mj-column-per-*` widths in a `min-width` media query, inline-block columns, a hero with `background` CSS and the legacy `background=` attribute, MSO/VML in conditional comments |
| `shipping-colspan.html` | A shipping notice styled from `<style>` (type, class, id, descendant selectors): `colspan`, `rowspan`, `cellpadding`, per-side borders |
| `dark-mode-meta.html` | `color-scheme` meta, `prefers-color-scheme: dark` rules, Outlook's `[data-ogsc]` selectors, light/dark logo swap |
| `complex-unsupported.html` | What the native renderer can't do: absolute and fixed positioning, CSS grid and flex, transforms, inline SVG, floats, a form, an iframe, video, script. The classifier should flag it |
| `notification-code.html` | A code-review notification: `<code>`, `<pre>`, uppercase tracked labels, bordered card |
| `calendar-invite.html` | A calendar invitation: label/value rows, pill RSVP links in a row |
| `typography-newsletter.html` | A text-led newsletter: serif and sans families, line heights, weights, italic, letter-spacing, text-transform, an id selector, a pull quote |
