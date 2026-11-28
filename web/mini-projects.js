import { button, card, field, form, node, row } from './mini-ui.js';
import { api, show } from './mini.js';

const roles = [['editor', 'Editor'], ['viewer', 'Viewer']];
const roleNames = {owner: 'Owner', editor: 'Editor', viewer: 'Viewer'};

export function projectSpaces(state) {
    const section = node('details', undefined, 'card');
    const content = node('div');
    let generation = 0;
    section.append(node('summary', 'Project spaces'), content);
    section.addEventListener('toggle', () => {
        if (section.open) void list();
        else {
            generation++;
            content.replaceChildren();
        }
    });

    const current = request => request === generation && section.open && section.isConnected;

    async function list() {
        const request = ++generation;
        const loading = node('p', 'Loading projects…', 'meta');
        loading.setAttribute('role', 'status');
        content.replaceChildren(loading);
        try {
            const {projects} = await api('list-projects');
            if (!current(request)) return;
            content.replaceChildren(
                node('p', 'Invite people and publish specific notes or task results to your project. Project items are shared with the members you review when publishing.'),
                node('p', `Your Telegram account ID is ${state.user}.`, 'meta'),
                form('Create project', [field('title', 'Project title', {max: 120})], async values => {
                    const {project} = await api('create-project', values);
                    if (current(request)) await open(project);
                }),
            );
            for (const project of projects) {
                const item = card(project.title, `${roleNames[project.role]} · ${project.joined ? 'Joined' : 'Invited'}`);
                if (project.joined) item.append(button('Open project', () => open(project.id)));
                else item.append(row(
                    button('Join project', async () => {
                        await api('project-member', {project: project.id, change: {accept: {member: project.membership, role: project.role}}});
                        if (current(request)) await open(project.id);
                    }),
                    button('Decline invitation', async () => {
                        await api('project-member', {project: project.id, change: {leave: {member: project.membership}}});
                        if (current(request)) await list();
                    }, true),
                ));
                content.append(item);
            }
        } catch (error) {
            if (current(request)) content.replaceChildren(node('p', error.message), button('Retry loading projects', list, true));
        }
    }

    async function open(project, offset = 0) {
        const request = ++generation;
        const back = button('Back to projects', list, true);
        const loading = node('p', 'Loading project…', 'meta');
        loading.setAttribute('role', 'status');
        const detail = node('div');
        detail.append(loading);
        content.replaceChildren(back, detail);
        back.focus();
        try {
            const {space, members, items, next_offset} = await api('project-detail', {project, offset});
            if (!current(request)) return;
            const reload = () => current(request) && open(project);
            const updateMember = async change => {
                await api('project-member', {project, change});
                await reload();
            };
            const roster = node('details', undefined, 'card');
            roster.append(node('summary', 'Project members'));
            for (const member of members) {
                const name = node('p', `${member.name} · ${roleNames[member.role]}${member.active ? '' : member.joined ? ' · Suspended' : ' · Invited'}`);
                roster.append(name);
                if (space.role === 'owner' && member.role !== 'owner') {
                    roster.append(form('Save role', [field('role', `Role for ${member.name}`, {choices: roles, value: member.role})], ({role}) => {
                        return updateMember({role: {member: member.id, role}});
                    }), button(`Revoke access for ${member.name}`, async () => {
                        if (window.confirm(`Revoke access for ${member.name}?`)) await updateMember({revoke: {member: member.id}});
                    }, true));
                }
            }
            if (space.role === 'owner') {
                roster.append(form('Invite member', [
                    field('user', 'Telegram account ID', {max: 24}),
                    field('role', 'Project role', {choices: roles, value: 'viewer'}),
                ], values => updateMember({invite: values})));
            } else {
                roster.append(button('Leave project', async () => {
                    if (!window.confirm('Leave this project? Any items you published stay available to their reviewed audience.')) return;
                    await api('project-member', {project, change: {leave: {member: space.membership}}});
                    if (current(request)) await list();
                }, true));
            }
            detail.replaceChildren(node('h2', space.title), roster);
            if (space.role !== 'viewer') detail.append(publisher(space, members, reload));
            if (items.length === 0) detail.append(node('p', 'No items shared with you here yet.', 'meta'));
            for (const item of items) detail.append(projectItem(project, item, reload));
            const pages = [];
            if (offset > 0) pages.push(button('Newer items', () => open(project, offset - 25), true));
            if (next_offset !== null) pages.push(button('Older items', () => open(project, next_offset), true));
            if (pages.length) detail.append(row(...pages));
        } catch (error) {
            if (current(request)) detail.replaceChildren(node('p', error.message),
                button('Retry loading project', () => open(project, offset), true));
        }
    }
    return section;
}

function publisher(space, members, reload) {
    const details = node('details', undefined, 'card');
    const preview = node('div');
    const results = node('div');
    const note = form('Review note', [
        field('title', 'Note title', {max: 120}),
        field('body', 'Note', {type: 'textarea', max: 8000}),
    ], async values => {
        review({...values, title: values.title.trim(), body: values.body.trim(), kind: 'note'}, 'Note version 1');
    });
    const task = form('Find task results', [field('query', 'Search finished tasks', {required: false, max: 800})], async ({query}) => {
        await findTasks(query);
    });
    details.append(node('summary', 'Publish to project'), note, task, results, preview);
    return details;

    async function findTasks(query, offset = 0) {
        const {items, next_offset} = await api('search-tasks', {query, offset});
        const choices = items.filter(item => ['completed', 'idle'].includes(item.state)).map(item => [item.id, item.title]);
        const pages = [];
        if (offset > 0) pages.push(button('Previous results', () => findTasks(query, offset - 20), true));
        if (next_offset !== null) pages.push(button('More results', () => findTasks(query, next_offset), true));
        results.replaceChildren();
        if (choices.length) {
            results.append(form('Review task result', [field('task', 'Task result', {choices})], async values => {
                const result = await api('preview-project-result', {project: space.id, task: values.task});
                review({...result, kind: 'task'}, `Task revision ${result.revision}${result.truncated ? ' · Excerpt' : ''}`);
            }));
        } else {
            results.append(node('p', 'No finished tasks on this page.', 'meta'));
        }
        if (pages.length) results.append(row(...pages));
    }

    function review(source, version) {
        const audience = members.filter(member => member.active);
        const summary = card(source.title, version);
        const text = node('p', source.body, 'memory-note');
        summary.append(text, node('p', `Share with ${audience.map(member => member.name).join(', ')}`));
        summary.append(row(button('Publish snapshot', async () => {
            const item = source.kind === 'task'
                ? {kind: 'task', task: source.task, revision: source.revision, event: source.event}
                : source;
            try {
                await api('publish-project-item', {
                    project: space.id,
                    review: {revision: space.revision, audience: audience.map(member => member.id)},
                    source: item,
                });
            } catch (error) {
                if (error.status === 409) await reload();
                throw error;
            }
            await reload();
            show('Snapshot published.');
        }), button('Cancel preview', () => preview.replaceChildren(), true)));
        preview.replaceChildren(summary);
    }
}

function projectItem(project, item, reload) {
    const section = card(item.title, item.kind === 'task' ? `Task result · Revision ${item.source_revision}` : 'Note · Version 1');
    section.append(node('p', item.body, 'memory-note'));
    if (item.done || item.assignee_name) {
        section.append(node('p', [item.done ? 'Completed' : 'Assigned', item.assignee_name].filter(Boolean).join(' · '), 'meta'));
    }
    const update = async change => {
        try {
            await api('project-item', {project, item: item.id, revision: item.revision, change});
        } catch (error) {
            if (error.status === 409) await reload();
            throw error;
        }
        await reload();
    };
    if (item.can_manage) {
        const choices = [['', 'Unassigned'], ...item.eligible.map(member => [member.id, member.name])];
        section.append(form('Save assignment', [field('member', 'Assign to', {
            choices, required: false, value: item.assignee ?? '',
        })], ({member}) => update({assign: {member: member || null}})));
    }
    const actions = [];
    if (item.can_complete) actions.push(button(item.done ? 'Reopen item' : 'Mark item complete', () => update({check: {done: !item.done}}), true));
    if (item.can_remove) actions.push(button('Withdraw snapshot', async () => {
        if (window.confirm('Withdraw this snapshot from the project?')) await update('withdraw');
    }, true));
    if (actions.length) section.append(row(...actions));
    return section;
}
