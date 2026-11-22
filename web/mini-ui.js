import { api, refresh, show } from './mini.js';

export function node(tag, text, className) {
    const element = document.createElement(tag);
    if (text !== undefined) element.textContent = text;
    if (className) element.className = className;
    return element;
}

export function button(label, callback, quiet = false) {
    const element = node('button', label, quiet ? 'quiet' : '');
    element.type = 'button';
    element.addEventListener('click', async () => {
        element.disabled = true;
        show('');
        try { await callback(); }
        catch (error) { show(error.message, true); }
        finally { element.disabled = false; }
    });
    return element;
}

export function card(title, message) {
    const element = node('section', undefined, 'card');
    if (title) element.append(node('h3', title));
    if (message) element.append(node('p', message));
    return element;
}

export function field(name, label, options = {}) {
    const element = node('label', label);
    const control = node(options.type === 'textarea' ? 'textarea' : options.choices ? 'select' : 'input');
    control.name = name;
    control.required = options.required ?? true;
    control.autocomplete = 'off';
    if (options.type && control.tagName === 'INPUT') control.type = options.type;
    if (options.max) control.maxLength = options.max;
    if (options.choices) for (const [value, label] of options.choices) control.add(new Option(label, value));
    if (options.value !== undefined) control.value = options.value;
    element.append(control);
    if (options.hint) element.append(node('span', options.hint, 'field-hint'));
    return element;
}

export function form(label, fields, submit) {
    const element = node('form');
    const save = node('button', label);
    save.type = 'submit';
    element.append(...fields, save);
    element.addEventListener('submit', async event => {
        event.preventDefault();
        save.disabled = true;
        show('');
        try { await submit(Object.fromEntries(new FormData(element)), element); }
        catch (error) { show(error.message, true); }
        finally { save.disabled = false; }
    });
    return element;
}

export function action(label, name, data, success, quiet = false) {
    return button(label, async () => {
        await api(name, data);
        await refresh();
        show(success);
    }, quiet);
}

export function row(...elements) {
    const wrapper = node('div', undefined, 'actions');
    wrapper.append(...elements);
    return wrapper;
}
