-- Secret shown to a user who is setting up two-factor authentication; it
-- becomes totp_secret once they enter a valid code from it.
ALTER TABLE users ADD COLUMN totp_pending TEXT;
