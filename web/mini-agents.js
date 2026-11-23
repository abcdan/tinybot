import { action, button, card, field, form, node, row } from './mini-ui.js';
import { api, content, refresh, show } from './mini.js';

const toolNames = {
    list_mcp_tools: 'Browse connector tools',
    call_mcp: 'Use connectors',
    browse: 'Read public websites',
    search_web: 'Search the web',
    task_files: 'Read task files'
};
export function agentForm(agent, proposal, pending) {
    const current = agent ? JSON.parse(agent.tools) : ['list_mcp_tools', 'call_mcp'];
    const fields = proposal?.fields || [{name: 'name', label: 'Name'}, {name: 'instructions', label: 'Instructions'}, {name: 'tools', label: 'Tools'}];
    const inputs = fields.filter(field => field.name !== 'tools').map(field =>
        fieldControl(field.name, field.label, agent?.[field.name] || proposal?.[field.name] || '', field.hint));
    if (!proposal || fields.some(field => field.name === 'tools')) {
        const groups = node('fieldset');
        groups.append(node('legend', 'Tools'));
        for (const [tool, label] of Object.entries(toolNames)) {
            const item = node('label');
            const checkbox = node('input');
            checkbox.type = 'checkbox';
            checkbox.name = 'tool';
            checkbox.value = tool;
            checkbox.checked = current.includes(tool);
            item.append(checkbox, document.createTextNode(label));
            groups.append(item);
        }
        inputs.push(groups);
    }
    return form(agent ? 'Save changes' : 'Save helper', inputs, async (data, element) => {
        await api('save-agent', {name: data.name, instructions: data.instructions,
            tools: element.querySelector('fieldset') ? new FormData(element).getAll('tool') : current,
            agent: agent?.id, pending});
        await refresh();
        show('Helper saved.');
    });
}

function fieldControl(name, label, value, hint) {
    return field(name, label, {value, hint, max: name === 'name' ? 80 : 2500, type: name === 'instructions' ? 'textarea' : 'text'});
}

export function agents(state) {
    const creator = card('Create a helper', 'Your Chief can delegate part of a task to a saved helper.');
    creator.append(agentForm(null, null));
    content.append(creator, node('h2', 'Saved helpers'));
    for (const agent of state.agents) {
        const owned = agent.owner === state.user;
        const item = card(agent.name, owned ? undefined : 'Shared with you');
        if (owned) {
            const settings = node('details');
            settings.append(node('summary', 'Edit helper'), agentForm(agent, null));
            const share = node('details');
            share.append(node('summary', 'Sharing'),
                node('p', 'Members can ask their Chief about published notes and explicitly shared records. Shared results can be sent to their model provider.'));
            const invite = node('pre');
            invite.hidden = true;
            share.append(button('Create one-time invitation', async () => {
                const result = await api('invite-agent', {agent: agent.id});
                invite.textContent = result.command;
                invite.hidden = false;
            }, true), invite);
            for (const [, member] of state.members.filter(([key]) => key === agent.id)) {
                share.append(row(node('span', 'Member ' + member, 'meta'),
                    action('Revoke', 'revoke-member', {agent: agent.id, member}, 'Member revoked.', true)));
            }
            share.append(form('Publish note', [field('body', 'Shared memory', {type: 'textarea', max: 1200})], async data => {
                await api('publish-memory', {...data, agent: agent.id});
                await refresh();
                show('Note published to this helper.');
            }));
            for (const [id, , body] of state.memory.filter(([, scope]) => scope === agent.id)) {
                share.append(node('p', body), action('Remove note', 'delete-memory', {id}, 'Note removed.', true));
            }
            const reads = state.connectors.flatMap(connector => connector.auth === 'pending' ? [] :
                (state.read_tools[connector.id] || []).map(tool => [JSON.stringify([connector.id, tool]), connector.name + ' · ' + tool]));
            if (reads.length) {
                share.append(form('Share exact read', [
                    field('read', 'Verified connector tool', {choices: reads}),
                    field('scope', 'Fixed arguments (JSON)', {type: 'textarea', max: 4000,
                        hint: 'For example, {"record_id":"123"}.'})
                ], async data => {
                    const [connector, tool] = JSON.parse(data.read);
                    await api('grant-connector', {agent: agent.id, connector, tool, scope: JSON.parse(data.scope)});
                    await refresh();
                    show('Read access granted for the fixed arguments.');
                }));
            }
            for (const [, connector, tool, scope] of state.grants.filter(([key]) => key === agent.id)) {
                share.append(node('p', scope), action('Remove record grant', 'revoke-connector',
                    {agent: agent.id, connector, tool}, 'Record grant removed.', true));
            }
            item.append(settings, share);
        }
        content.append(item);
    }
}
