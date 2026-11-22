import { createHash } from 'node:crypto';

export class InputError extends Error {}

export function requireInput(ok, message) {
    if (!ok) throw new InputError(message);
}

export function publicUrl(value) {
    requireInput(typeof value === 'string' && value.length <= 2048, 'Expected a public HTTPS URL.');
    let url;
    try { url = new URL(value); }
    catch { throw new InputError('Expected a public HTTPS URL.'); }
    requireInput(url.protocol === 'https:' && !url.port && !url.hash && !url.username && !url.password,
        'Expected a public HTTPS URL on port 443.');
    return url;
}

export async function describe(page, index) {
    const url = publicUrl(page.url());
    const data = await page.evaluate(formIndex => {
        const form = document.forms[formIndex];
        if (!form) return { error: 'No form exists at that position.' };
        if (form.method !== 'post' || form.enctype !== 'application/x-www-form-urlencoded' ||
            !['', '_self'].includes(form.target)) {
            return { error: 'This form needs a same-page, URL-encoded HTTPS POST.' };
        }
        const target = new URL(form.action, document.baseURI);
        if (target.origin !== location.origin || target.hash || target.username || target.password) {
            return { error: 'This form submits to a different origin or an unsupported destination.' };
        }
        const fields = [];
        const hidden = [];
        const constraints = [];
        const names = new Set();
        for (const input of form.elements) {
            if (input.matches(':disabled')) continue;
            const tag = input.tagName;
            const kind = tag === 'TEXTAREA' ? 'textarea' : tag === 'SELECT' ? 'select' : input.type;
            if (['BUTTON', 'FIELDSET', 'OUTPUT'].includes(tag) || ['button', 'submit', 'reset', 'image'].includes(kind)) continue;
            if (!['INPUT', 'TEXTAREA', 'SELECT'].includes(tag)) return { error: 'The form has a custom control.' };
            if (!input.name) {
                if (input.required) return { error: 'The site has a required field without a name.' };
                continue;
            }
            if (input.name.length > 96 || names.has(input.name)) return { error: 'The form field names are ambiguous.' };
            names.add(input.name);
            const description = [input.name, input.id, input.autocomplete,
                ...(Array.from(input.labels || []).map(label => label.textContent))].join(' ');
            if (kind === 'password' || /password|passcode|one.time.code|otp|cvv|cc.csc|cc.number|api.key/i.test(description)) {
                return { error: 'Enter sign-in or payment credentials on the provider’s own page.' };
            }
            if (kind === 'hidden') {
                hidden.push(input.name);
                continue;
            }
            if (!['text', 'textarea', 'email', 'tel', 'date', 'number', 'select', 'checkbox'].includes(kind) ||
                input.multiple || input.readOnly || input.form !== form) {
                return { error: 'The form contains an unsupported control.' };
            }
            const options = kind === 'select' ?
                [...input.options].filter(option => !option.matches(':disabled') && option.value !== '').map(option => option.value) : [];
            if (options.length > 50 || new Set(options).size !== options.length || options.some(option => option.length > 160)) {
                return { error: 'The site has unsupported select options.' };
            }
            const label = (input.labels?.[0]?.textContent || input.getAttribute('aria-label') || input.name)
                .replace(/\s+/g, ' ').trim().slice(0, 160) || input.name;
            fields.push({name: input.name, label, type: kind, required: input.required, options, private: true});
            constraints.push([input.name, input.min, input.max, input.step, input.minLength, input.maxLength, input.pattern,
                kind === 'checkbox' ? input.value : null]);
        }
        if (fields.length < 1 || fields.length > 24) return { error: 'The number of form fields is unsupported.' };
        return { destination: target.href, fields, hidden, constraints };
    }, index);
    requireInput(!data.error, data.error);
    const destination = publicUrl(data.destination);
    requireInput(url.origin === destination.origin, 'The site changed the form destination.');
    const fingerprint = createHash('sha256').update(JSON.stringify([url.href, index, data])).digest('hex');
    return {page: url.href, destination: destination.href, index, fingerprint, fields: data.fields};
}

export async function populate(page, confirmed, values) {
    const observed = await describe(page, confirmed.index);
    requireInput(observed.fingerprint === confirmed.fingerprint && observed.destination === confirmed.destination,
        'The site changed the form. Request a fresh form.');
    requireInput(values && typeof values === 'object' && !Array.isArray(values) &&
        Object.keys(values).length <= observed.fields.length &&
        Object.entries(values).every(([name, value]) =>
            typeof value === 'string' && value.length <= 4096 && observed.fields.some(field => field.name === name)),
        'The submitted values no longer match the form.');
    const valid = await page.evaluate(({ index, values, fields }) => {
        const form = document.forms[index];
        for (const field of fields) {
            const input = form.elements.namedItem(field.name);
            if (!input || input.matches(':disabled')) return false;
            if (!Object.hasOwn(values, field.name) || values[field.name] === '') {
                // Optional visible fields must never silently send an existing value.
                if (!field.required) input.disabled = true;
                continue;
            }
            if (field.type === 'checkbox') input.checked = values[field.name] === 'true';
            else input.value = values[field.name];
            if (field.type !== 'checkbox' && input.value !== values[field.name]) return false;
            if (input.minLength >= 0 && input.value.length < input.minLength) return false;
            if (input.maxLength >= 0 && input.value.length > input.maxLength) return false;
        }
        return HTMLFormElement.prototype.checkValidity.call(form);
    }, {index: observed.index, values, fields: observed.fields});
    requireInput(valid, 'The website rejected one or more field values. Request a fresh form.');
    return observed;
}
