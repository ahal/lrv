import { el } from './dom';
import { openModal } from './modal';
import { fetchJSON } from './api';
import { commentKey, parseQueuedReviewNoteBody } from './dialog-methods';
import type { ReviewComment } from './comments';
import type { AppContext, PhabStatus, PublishTabState, ReviewAction } from './types/app';

export class PublishDialogMethods {
  declare commentManager: AppContext['commentManager'];
  declare seriesInfo: AppContext['seriesInfo'];
  declare publishedCommentKeys: Set<string>;
  declare publishTabStates: Map<string, PublishTabState>;
  declare phabStatus: Promise<PhabStatus> | null;
  declare renderSubmitComments: (body: HTMLElement, comments: ReviewComment[]) => Promise<void>;

  prefetchPhabricator(): Promise<PhabStatus> {
    this.phabStatus ??= fetchJSON<PhabStatus>('/api/phabricator').catch((error) => {
      this.phabStatus = null;
      throw error;
    });
    return this.phabStatus;
  }

  async showPublishDialog() {
    let active = '';
    let publish: () => Promise<void> = async () => {};
    const closeButton = el('button', { className: 'btn-secondary', text: 'Close' });
    const publishButton = el('button', {
      className: 'btn-primary publish-revision',
      text: 'Publish',
    });
    publishButton.disabled = true;
    const { body, close } = openModal({
      title: 'Publish to Phabricator',
      titleId: 'publish-title',
      modalClass: 'submit-review-modal publish-dialog',
      footerContent: [closeButton, publishButton],
      onKeydown: (event) => {
        if (event.key === 'Enter' && (event.ctrlKey || event.metaKey)) {
          event.preventDefault();
          void publish();
        }
      },
    });
    closeButton.onclick = close;
    body.appendChild(el('p', { text: 'Validating Phabricator revisions…' }));
    let status: PhabStatus;
    try {
      status = await this.prefetchPhabricator();
    } catch (error) {
      body.replaceChildren(el('div', { className: 'submit-error', text: String(error) }));
      return;
    }
    body.replaceChildren(el('p', { text: status.user ? `as @${status.user}` : '' }));
    if (status.error) {
      body.appendChild(el('div', { className: 'submit-error', text: status.error }));
    }
    const originals = this.commentManager
      .getComments()
      .filter((c) => !this.publishedCommentKeys.has(commentKey(c)));
    const forRevision = (id: string) => {
      const revision = status.revisions.find((r) => r.id === id)!;
      return originals.filter(
        (c) => revision.commit_idxs.includes(c.commit_idx ?? 0) && !isAddress(c),
      );
    };
    const unassigned = originals.filter(
      (c) =>
        isAddress(c) || !status.revisions.some((r) => r.commit_idxs.includes(c.commit_idx ?? 0)),
    );
    const tabs = el('div', { className: 'publish-tabs', attrs: { role: 'tablist' } });
    const panel = el('div', { className: 'publish-panel', attrs: { role: 'tabpanel' } });
    body.append(tabs, panel);
    active = status.revisions.find((r) => !r.published)?.id ?? status.revisions[0]?.id ?? '';
    let rendering = 0;
    const render = () => {
      const generation = ++rendering;
      tabs.replaceChildren();
      tabs.hidden = status.revisions.length + Number(unassigned.length > 0) <= 1;
      for (const revision of status.revisions) {
        const badge = revision.published
          ? '✓'
          : !revision.publishable
            ? '⚠'
            : `· ${forRevision(revision.id).length}`;
        const tab = el('button', {
          className: 'publish-tab',
          text: `${revision.id} ${badge}`,
          attrs: {
            role: 'tab',
            'aria-selected': String(active === revision.id),
            title: revision.reason ?? '',
          },
        });
        tab.onclick = () => {
          active = revision.id;
          render();
        };
        tabs.appendChild(tab);
      }
      if (unassigned.length) {
        const tab = el('button', {
          className: 'publish-tab',
          text: `Not published · ${unassigned.length}`,
          attrs: { role: 'tab', 'aria-selected': String(active === 'unassigned') },
        });
        tab.onclick = () => {
          active = 'unassigned';
          render();
        };
        tabs.appendChild(tab);
      }
      panel.replaceChildren();
      publishButton.hidden = active === 'unassigned';
      if (active === 'unassigned') {
        panel.appendChild(
          el('p', {
            text: 'These comments are for the agent. Close this dialog and choose Submit To.. → Agent.',
          }),
        );
        void this.renderSubmitComments(panel, unassigned);
        return;
      }
      const revision = status.revisions.find((r) => r.id === active);
      if (!revision) {
        return;
      }
      let state = this.publishTabStates.get(active);
      if (!state) {
        state = { action: 'comment', overall: '' };
        this.publishTabStates.set(active, state);
      }
      const tabState = state;
      const comments = forRevision(active);
      const updateButton = () => {
        publishButton.disabled =
          !!status.error ||
          !revision.publishable ||
          revision.published ||
          !!tabState.busy ||
          (!comments.length && !tabState.overall.trim() && tabState.action === 'comment');
        publishButton.textContent = revision.published
          ? 'Published ✓'
          : tabState.busy
            ? 'Publishing…'
            : `Publish ${revision.id}`;
      };
      panel.appendChild(
        el('a', {
          className: 'auto-link',
          text: `${revision.id} ${revision.title} ↗`,
          attrs: { href: revision.url, target: '_blank', rel: 'noopener noreferrer' },
        }),
      );
      panel.appendChild(
        el('p', {
          text: `Commit ${revision.commit_idxs.map((i) => i + 1).join(', ')}/${this.seriesInfo?.commits.length ?? 1}`,
        }),
      );
      if (revision.reason) {
        panel.appendChild(el('div', { className: 'submit-error', text: revision.reason }));
      }
      if (revision.closed) {
        panel.appendChild(
          el('p', {
            className: 'submit-publish-note',
            text: 'This revision is closed or abandoned.',
          }),
        );
      }
      const actions = el('div', {
        className: 'publish-verdict',
        attrs: { role: 'group', 'aria-label': 'Verdict' },
      });
      const options: Array<[ReviewAction, string]> = [
        ['comment', 'Comment'],
        ['accept', 'Accept'],
        ['request-changes', 'Request Changes'],
      ];
      for (const [value, label] of options) {
        const button = el('button', {
          text: label,
          attrs: {
            'aria-pressed': String(tabState.action === value),
            title:
              revision.own && value !== 'comment'
                ? 'This is your own revision; you can only comment.'
                : '',
          },
        });
        button.disabled =
          revision.published || !!tabState.busy || (revision.own && value !== 'comment');
        button.onclick = () => {
          tabState.action = value;
          render();
        };
        actions.appendChild(button);
      }
      panel.appendChild(actions);
      if (revision.own) {
        panel.appendChild(
          el('p', {
            className: 'submit-publish-note',
            text: `You are the author of ${revision.id}, so Phabricator only lets you comment.`,
          }),
        );
      }
      const input = el('textarea', {
        className: 'submit-summary-input',
        attrs: {
          'aria-label': `Overall comment for ${revision.id}`,
          placeholder: `Overall comment for ${revision.id}`,
        },
      });
      input.value = tabState.overall;
      input.disabled = revision.published || !!tabState.busy;
      input.oninput = () => {
        tabState.overall = input.value;
        updateButton();
      };
      panel.appendChild(input);
      if (tabState.error) {
        panel.appendChild(el('div', { className: 'submit-error', text: tabState.error }));
      }
      const preview = el('div');
      panel.appendChild(preview);
      const plainComments = comments.map(unwrapReply);
      void this.renderSubmitComments(preview, plainComments).then(() => {
        if (generation !== rendering) {
          preview.remove();
        }
      });
      panel.appendChild(
        el('p', {
          className: 'submit-publish-note',
          text: `Any unsubmitted drafts you have on ${revision.id} are published.`,
        }),
      );
      updateButton();
      publish = async () => {
        if (publishButton.disabled || publishButton.hidden) {
          return;
        }
        tabState.busy = true;
        delete tabState.error;
        render();
        try {
          const response = await fetch('/api/phabricator/publish', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
              revision: revision.id,
              comments: plainComments,
              overall_comment: tabState.overall,
              action: tabState.action,
            }),
          });
          if (!response.ok) {
            throw new Error((await response.text()).trim() || `HTTP ${response.status}`);
          }
          revision.published = true;
          comments.forEach((c) => this.publishedCommentKeys.add(commentKey(c)));
          const next = status.revisions.find((r) => r.publishable && !r.published);
          if (!next) {
            // Everything that can be published has been: there's nothing left to do here.
            setTimeout(close, 1000);
          }
          active = next?.id ?? revision.id;
        } catch (error) {
          tabState.error = `Failed to publish review: ${error instanceof Error ? error.message : error}`;
        } finally {
          tabState.busy = false;
          render();
        }
      };
    };
    publishButton.onclick = () => void publish();
    render();
  }
}

function isAddress(comment: ReviewComment): boolean {
  const parsed = parseQueuedReviewNoteBody(comment.body);
  return parsed !== null && !parsed.instruction;
}

function unwrapReply(comment: ReviewComment): ReviewComment {
  const parsed = parseQueuedReviewNoteBody(comment.body);
  return parsed?.instruction ? { ...comment, body: parsed.instruction } : comment;
}
