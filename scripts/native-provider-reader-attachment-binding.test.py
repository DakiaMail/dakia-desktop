#!/usr/bin/env python3
import base64
import hashlib
import importlib.util
import unittest
from pathlib import Path

MODULE = Path(__file__).with_name("native-provider-reader-attachment-binding.py")
spec = importlib.util.spec_from_file_location("binding", MODULE)
binding = importlib.util.module_from_spec(spec)
spec.loader.exec_module(binding)


def fixture(*, message_id=binding.MESSAGE_ID, safe_first=False):
    unsafe = [
        b'Content-Type: application/octet-stream; name="quarterly-report.pdf.command"',
        b'Content-Disposition: attachment; filename="../../quarterly-report.pdf.command"',
        b'Content-Transfer-Encoding: base64', b'', base64.b64encode(binding.UNSAFE_BYTES),
    ]
    safe = [
        b'Content-Type: text/plain; name="meeting-notes.txt"',
        b'Content-Disposition: attachment; filename="meeting-notes.txt"',
        b'Content-Transfer-Encoding: base64', b'', base64.b64encode(binding.SAFE_BYTES),
    ]
    parts = [safe, unsafe] if safe_first else [unsafe, safe]
    lines = [
        b'From: Fictional Provider <provider@example.test>',
        b'To: Fictional Reader <fictional-reader@localhost>',
        f'Subject: {binding.SUBJECT}'.encode(),
        f'Message-ID: {message_id}'.encode(),
        b'MIME-Version: 1.0',
        b'Content-Type: multipart/mixed; boundary="fixture-boundary"', b'',
        b'--fixture-boundary', b'Content-Type: text/html; charset=utf-8', b'', b'<p>body</p>',
    ]
    for part in parts: lines += [b'--fixture-boundary', *part]
    lines += [b'--fixture-boundary--', b'']
    return b'\r\n'.join(lines)


def collision_fixture():
    return b'\r\n'.join([
        b'From: Fictional Provider <provider@example.test>',
        b'To: Fictional Reader Two <fictional-reader-two@localhost>',
        f'Subject: {binding.COLLISION_SUBJECT}'.encode(),
        f'Message-ID: {binding.MESSAGE_ID}'.encode(),
        b'MIME-Version: 1.0',
        b'Content-Type: multipart/mixed; boundary="account-two-boundary"', b'',
        b'--account-two-boundary', b'Content-Type: text/plain; charset=utf-8', b'', binding.COLLISION_BODY.encode(),
        b'--account-two-boundary', b'Content-Type: text/plain; name="account-two-notes.txt"',
        b'Content-Disposition: attachment; filename="account-two-notes.txt"',
        b'Content-Transfer-Encoding: base64', b'', base64.b64encode(binding.COLLISION_ATTACHMENT_BYTES),
        b'--account-two-boundary--', b'',
    ])


class FakeClient:
    def __init__(self, raw=None, *, search=b'3', selected_uidv=b'77', status_uidv=77, uidnext=4, fetch_uid=3, count=b'3', flags=b'', full_section=b'BODY[]'):
        self.raw = raw or fixture()
        self.search = search
        self.selected_uidv = selected_uidv
        self.status_uidv = status_uidv
        self.uidnext = uidnext
        self.fetch_uid = fetch_uid
        self.count = count
        self.flags = flags
        self.full_section = full_section
        self.commands = []
    def select(self, mailbox, readonly=False):
        self.commands.append(('EXAMINE', mailbox, readonly)); return 'OK', [self.count]
    def response(self, key):
        return key, [self.selected_uidv]
    def status(self, mailbox, fields):
        self.commands.append(('STATUS', mailbox, fields)); return 'OK', [f'INBOX (UIDNEXT {self.uidnext} UIDVALIDITY {self.status_uidv})'.encode()]
    def uid(self, command, *args):
        self.commands.append((command, *args))
        if command == 'SEARCH': return 'OK', [self.search]
        if args[-1] == '(UID FLAGS INTERNALDATE BODY.PEEK[])':
            header=(b'3 (UID %d FLAGS (' % self.fetch_uid + self.flags + b') INTERNALDATE "22-Oct-2026 12:00:00 +0000" ' + self.full_section + b' {' + str(len(self.raw)).encode() + b'}')
            return 'OK', [(header, self.raw), b')']
        if args[-1] == '(UID FLAGS)': return 'OK', [b'3 (UID 3 FLAGS (' + self.flags + b'))']
        raise AssertionError((command,args))


class BindingTests(unittest.TestCase):
    def test_faithful_uid3_derives_safe_section_three_and_raw_prefix(self):
        client=FakeClient()
        raw, artifact=binding.build_binding(client, account_id=binding.ACCOUNT_ID)
        self.assertEqual(raw, fixture())
        self.assertEqual(artifact['uidValidity'],77)
        self.assertEqual(artifact['rawSha256'],hashlib.sha256(raw).hexdigest())
        self.assertEqual(artifact['rawBytes'],len(raw))
        self.assertEqual(artifact['fixtureProvenance']['kind'],'generated-fictional')
        self.assertEqual(artifact['attachment']['section'],'3')
        self.assertEqual(artifact['attachment']['id'],f'{binding.ACCOUNT_ID}:INBOX:3:mime-v1:3')
        self.assertEqual(artifact['attachment']['decodedBytes'],32)
        self.assertEqual(artifact['attachment']['decodedSha256'],binding.SAFE_SHA256)
        offset=artifact['attachment']['rawOffset']; length=artifact['attachment']['wirePrefixLength']
        self.assertEqual(hashlib.sha256(raw[offset:offset+length]).hexdigest(),artifact['attachment']['wirePrefixSha256'])
        self.assertEqual(artifact['unsafeAttachment']['section'],'2')
        self.assertEqual(artifact['unsafeAttachment']['decodedBytes'],28)
        self.assertEqual(artifact['unsafeAttachment']['decodedSha256'],binding.UNSAFE_SHA256)
        self.assertEqual(client.commands[0],('EXAMINE','INBOX',True))
        self.assertIn(('FETCH','3','(UID FLAGS INTERNALDATE BODY.PEEK[])'),client.commands)

    def test_duplicate_or_wrong_uid_is_rejected(self):
        with self.assertRaisesRegex(RuntimeError,'absent, duplicated, or routed'):
            binding.build_binding(FakeClient(search=b'3 4'),account_id=binding.ACCOUNT_ID)
        with self.assertRaisesRegex(RuntimeError,'wrong UID'):
            binding.build_binding(FakeClient(fetch_uid=4),account_id=binding.ACCOUNT_ID)

    def test_wrong_message_identity_is_rejected(self):
        with self.assertRaisesRegex(RuntimeError,'Message-ID changed'):
            binding.build_binding(FakeClient(raw=fixture(message_id='<wrong@localhost>')),account_id=binding.ACCOUNT_ID)

    def test_attachment_order_and_safe_section_are_not_assumed_away(self):
        with self.assertRaisesRegex(RuntimeError,'unsafe attachment identity changed|safe attachment identity changed'):
            binding.build_binding(FakeClient(raw=fixture(safe_first=True)),account_id=binding.ACCOUNT_ID)

    def test_epoch_or_uidnext_drift_is_rejected(self):
        with self.assertRaisesRegex(RuntimeError,'epoch or UIDNEXT changed'):
            binding.build_binding(FakeClient(status_uidv=78),account_id=binding.ACCOUNT_ID)
        with self.assertRaisesRegex(RuntimeError,'epoch or UIDNEXT changed'):
            binding.build_binding(FakeClient(uidnext=5),account_id=binding.ACCOUNT_ID)

    def test_empty_mailbox_and_read_side_effect_are_rejected(self):
        with self.assertRaisesRegex(RuntimeError,'exactly three messages'):
            binding.build_binding(FakeClient(count=b'0'),account_id=binding.ACCOUNT_ID)
        with self.assertRaisesRegex(RuntimeError,'read-neutral'):
            binding.build_binding(FakeClient(flags=br'\Seen'),account_id=binding.ACCOUNT_ID)
        with self.assertRaisesRegex(RuntimeError,'read-neutral'):
            binding.build_binding(FakeClient(flags=br'\sEeN'),account_id=binding.ACCOUNT_ID)

    def test_missing_flags_wrong_body_section_and_oversize_are_rejected(self):
        with self.assertRaisesRegex(RuntimeError,'wrong body section|read-neutral'):
            binding.raw_fetch([(b'3 (UID 3 BODY[] {1}', b'x'), b')'])
        with self.assertRaisesRegex(RuntimeError,'literal length is inconsistent'):
            binding.raw_fetch([(b'3 (UID 3 FLAGS () BODY[] {2}', b'x'), b')'])
        with self.assertRaisesRegex(RuntimeError,'wrong body section'):
            binding.build_binding(FakeClient(full_section=b'BODY[HEADER]'),account_id=binding.ACCOUNT_ID)
        with self.assertRaisesRegex(RuntimeError,'bounded fixture limit'):
            binding.build_binding(FakeClient(raw=b'x' * (binding.MAX_RAW_BYTES + 1)),account_id=binding.ACCOUNT_ID)

    def test_post_fetch_missing_flags_and_mixed_case_seen_are_rejected(self):
        self.assertFalse(binding.unread_uid3_flags([b'3 (UID 3 FLAGS (\\sEeN))']))
        with self.assertRaisesRegex(RuntimeError,'omitted FLAGS'):
            binding.unread_uid3_flags([b'3 (UID 3 INTERNALDATE "22-Oct-2026 12:00:00 +0000")'])
        with self.assertRaisesRegex(RuntimeError,'exactly one UID3'):
            binding.unread_uid3_flags([b'30 (UID 30 FLAGS ())'])

    def test_malformed_message_and_missing_attachment_are_rejected(self):
        malformed = fixture().replace(b'--fixture-boundary--', b'--wrong-boundary--')
        with self.assertRaisesRegex(RuntimeError,'attachment 3 encoded bytes were not unique|exactly two attachment candidates'):
            binding.build_binding(FakeClient(raw=malformed),account_id=binding.ACCOUNT_ID)
        missing = fixture().replace(base64.b64encode(binding.SAFE_BYTES), b'')
        with self.assertRaises(RuntimeError):
            binding.build_binding(FakeClient(raw=missing),account_id=binding.ACCOUNT_ID)

    def test_account_route_is_embedded_in_attachment_ids(self):
        _, artifact=binding.build_binding(FakeClient(),account_id=binding.ACCOUNT_ID)
        self.assertTrue(artifact['attachment']['id'].startswith(binding.ACCOUNT_ID+':'))
        self.assertNotEqual(artifact['attachment']['id'].split(':')[0], '62a9320c-04f5-47c2-b4fa-d4952f0b6122')

    def test_collision_account_proves_same_id_but_distinct_read_neutral_body(self):
        client=FakeClient(raw=collision_fixture())
        result=binding.verify_collision_account(client)
        self.assertEqual(result['attachmentFilename'],binding.COLLISION_ATTACHMENT_NAME)
        self.assertEqual(result['attachmentSha256'],binding.COLLISION_ATTACHMENT_SHA256)
        self.assertEqual(client.commands,[('EXAMINE','INBOX',True),('SEARCH',None,'HEADER','Message-ID',binding.MESSAGE_ID),('FETCH','3','(UID FLAGS INTERNALDATE BODY.PEEK[])'),('FETCH','3','(UID FLAGS)')])
        with self.assertRaisesRegex(RuntimeError,'absent or duplicated'):
            binding.verify_collision_account(FakeClient(raw=collision_fixture(),search=b'3 4'))
        with self.assertRaisesRegex(RuntimeError,'subject changed'):
            binding.verify_collision_account(FakeClient(raw=fixture()))


if __name__ == '__main__': unittest.main()
