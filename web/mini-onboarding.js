import { button, field, form, node, row } from './mini-ui.js';

export function onboarding(view, state, { api, finish, cancel, openLink }) {
    let displayName = state.display_name ?? '';
    let chiefName = state.chief_name ?? '';
    let key = '';
    let active = true;
    let keyInput;

    function dispose() {
        active = false;
        key = '';
        if (keyInput) keyInput.value = '';
    }

    function screen(step, title, subtitle) {
        const progress = node('ol', undefined, 'onboarding-progress');
        progress.setAttribute('aria-label', 'Setup progress');
        progress.setAttribute('role', 'list');
        for (const label of ['Profile', 'Model']) {
            const item = node('li', label);
            if (label === step) item.setAttribute('aria-current', 'step');
            progress.append(item);
        }

        const panel = node('section', undefined, 'card onboarding-screen');
        const heading = node('h2', title);
        heading.tabIndex = -1;
        const status = node('p', '', 'onboarding-status');
        status.setAttribute('role', 'status');
        status.hidden = true;
        panel.append(progress, heading, node('p', subtitle));
        return {panel, heading, status};
    }

    function mount({panel, heading}, focus) {
        view.replaceChildren(panel);
        if (focus) heading.focus();
    }

    function profile(focus = false) {
        const page = screen('Profile', 'Set up TinyBot', 'Name your Chief and connect OpenRouter. New service connections can act on your chat instructions and saved rules.');
        const name = field('display_name', 'Your name', {value: displayName, max: 128});
        const chief = field('chief_name', 'Agent name', {value: chiefName, max: 128});
        const secret = field('key', 'OpenRouter API key', {type: 'password', max: 8192});
        keyInput = secret.querySelector('input');
        keyInput.spellcheck = false;
        keyInput.autocapitalize = 'none';

        const keyLink = node('a', 'Get an OpenRouter API key', 'onboarding-key-link');
        keyLink.href = 'https://openrouter.ai/settings/keys';
        keyLink.target = '_blank';
        keyLink.rel = 'noopener noreferrer';
        keyLink.addEventListener('click', event => {
            if (event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
            if (openLink(keyLink.href)) event.preventDefault();
        });

        const setup = form('Continue', [name, chief, secret, keyLink, page.status], async (data, element) => {
            displayName = data.display_name.trim();
            chiefName = data.chief_name.trim();
            const candidate = data.key.trim();
            if (![displayName, chiefName].every(validName)) {
                updateStatus(page.status, 'Enter both names using up to 64 characters.', true);
                return;
            }
            if (!candidate) {
                updateStatus(page.status, 'Enter your OpenRouter API key.', true);
                keyInput.focus();
                return;
            }
            element.setAttribute('aria-busy', 'true');
            updateStatus(page.status, 'Checking OpenRouter…');
            try {
                const {models} = await api('onboarding-models', {key: candidate});
                if (!active) return;
                if (!Array.isArray(models) || !models.length) {
                    updateStatus(page.status, 'This key has no available tool-capable models.', true);
                    return;
                }
                key = candidate;
                keyInput.value = '';
                keyInput = undefined;
                choose(models);
            } catch (error) {
                if (active) updateStatus(page.status, error.message, true);
            } finally { element.removeAttribute('aria-busy'); }
        });
        setup.append(button('Cancel setup', cancel, true));
        page.panel.append(setup);
        mount(page, focus);
    }

    function choose(models) {
        const page = screen('Model', 'Choose a model', `Pick the OpenRouter model ${chiefName} will use for tasks.`);
        const search = field('query', 'Search models', {type: 'search', required: false});
        const choice = field('model', 'Task model', {choices: [['', 'Choose a model']]});
        const select = choice.querySelector('select');
        const query = search.querySelector('input');
        const results = node('p', '', 'meta');
        results.setAttribute('role', 'status');
        const labels = new Map();
        for (const {name} of models) labels.set(name, (labels.get(name) ?? 0) + 1);

        function updateChoices() {
            const selected = select.value;
            const term = query.value.trim().toLowerCase();
            const matching = models.filter(({name, id}) => `${name} ${id}`.toLowerCase().includes(term));
            select.replaceChildren(new Option('Choose a model', ''));
            for (const {id, name} of matching) {
                select.add(new Option(name && labels.get(name) === 1 ? name : id, id));
            }
            select.value = matching.some(({id}) => id === selected) ? selected : '';
            select.disabled = matching.length === 0;
            submit.disabled = !select.value;
            results.textContent = matching.length === 0 ? 'No models match your search.' : '';
            results.hidden = matching.length !== 0;
        }

        const setup = form('Finish setup', [search, choice, results, page.status], async (data, element) => {
            if (!models.some(({id}) => id === data.model)) return;
            element.setAttribute('aria-busy', 'true');
            query.disabled = true;
            select.disabled = true;
            back.disabled = true;
            updateStatus(page.status, 'Saving your setup…');
            try {
                await api('complete-onboarding', {display_name: displayName, chief_name: chiefName, key, model: data.model});
                if (!active) return;
                dispose();
                await finish();
            } catch (error) {
                if (active) {
                    updateStatus(page.status, error.message, true);
                    if (error.status === 409) {
                        dispose();
                        await finish();
                    }
                }
            } finally {
                element.removeAttribute('aria-busy');
                query.disabled = false;
                select.disabled = false;
                back.disabled = false;
            }
        });
        const back = button('Back', () => {
            key = '';
            profile(true);
        }, true);
        const submit = setup.querySelector('button[type="submit"]');
        query.addEventListener('input', updateChoices);
        select.addEventListener('change', () => { submit.disabled = !select.value; });
        updateChoices();
        setup.append(back);
        page.panel.append(setup);
        mount(page, true);
    }

    profile();
    return dispose;
}

function validName(value) {
    return value.length > 0 && [...value].length <= 64 && !/[\x00-\x1f\x7f-\x9f]/u.test(value);
}

function updateStatus(status, text, isError = false) {
    status.textContent = text;
    status.hidden = false;
    status.classList.toggle('onboarding-error', isError);
}
