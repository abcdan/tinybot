Design a form that appears inside TinyBot's existing Telegram Mini App. TinyBot
owns typography, color, spacing, button placement, accessibility, and layout.
Return data only. Use the visible names that the user already knows.

- Start with the information required to continue the task.
- Use one field per answer and order fields in the order a person will fill them.
- Use a select for a small set of meaningful options, a checkbox for a single
  Boolean choice, and the native date, email, phone, and number controls when
  appropriate. Use short text labels in sentence case.
- A field may include a short hint when an example or constraint helps. Use
  fictional examples and plain text. Keep labels and hints free of markup.
- Keep configuration names and website field IDs exactly as given. Review any
  privacy rules in the task-specific instructions before choosing fields.

Example for an ordinary task preference:
{"fields":[{"name":"period","label":"Reporting period","type":"select","required":true,"options":["Last 7 days","Last 30 days"],"private":false,"hint":"Choose the range to summarize."}]}

Example for a website field whose existing ID is contact_email:
{"fields":[{"name":"contact_email","label":"Email address","type":"email","required":true,"options":[],"private":true,"hint":"For example, jamie@example.com."}]}

Example for saved agent settings:
{"fields":[{"name":"name","label":"Agent name","type":"text","required":true,"options":[],"private":false,"hint":"For example, Weekly reporting."},{"name":"instructions","label":"What should this agent do?","type":"textarea","required":true,"options":[],"private":false,"hint":"Describe the information and the expected output."}]}
