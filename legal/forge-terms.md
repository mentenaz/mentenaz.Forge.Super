# Forge Terms of Use (draft v0.1)

21 Nov 2026 · @Francois

## Key points

- Forge is provided "as is". We don't promise it is error-free or fits your purpose.
- Analysis reports and template checks are information only. They can be incomplete or wrong, and "nothing found" does not mean "safe".
- You are responsible for any code, template or dependency you open, install or run, and for what it does.
- Forge needs no account and does not send your code or files anywhere. A small file on your computer records that you accepted these terms.
- We may update these terms. You are asked to accept again when a change is significant.
- Nothing here removes rights that the law does not allow to be removed.

Draft v0.1. A qualified lawyer should review it before release. Text in [square brackets] is still to be filled in.

## 1. What these terms cover

These terms apply to your use of Forge, including its editor, code-analysis engine, template engine and the command-line tools that come with them (together, "Forge"). They are between you and Francois Jacobus Huyzers, South Africa ("we"). By choosing "Accept" in the app you agree to them. If you do not agree, do not use Forge. Forge's source code is licensed separately under the GNU General Public License, version 3 or later, except where a crate or file states another licence (some are under the Apache License 2.0 or the MIT licence); for the code itself, that licence applies where it differs from these terms.

## 2. Using Forge

You may use Forge for lawful purposes, personal or commercial, as the licence allows. You do not need an account. Do not use Forge to harm others, to attack systems you do not own or have permission to test, or to break the law. Forge keeps your code, projects and reports on your computer. We do not collect them.

## 3. Analysis reports and templates

An analysis report is information, not a security assessment, audit or certification. The analysis engine reads code without running it and reports what that code appears able to do, and what it could not trace. A report can be incomplete or wrong, and code can be written to avoid detection. A report with no findings means nothing was found in what could be analysed. It does not mean the code is safe. Text in a report that comes from the analysed code is data, not instructions.

The template engine creates files from templates, including templates written by other people. Forge can check a template's structure and warn about some risks, but it cannot promise that a template is safe or correct. Review what a template creates before you use it.

## 4. Running code and templates is your responsibility

You decide what to open, install, run or trust. Opening a folder, installing dependencies or applying a template can run code, including code written by third parties. Forge's warnings and restricted mode reduce some risk but do not remove it. We are not responsible for what code, templates or dependencies you choose to run, or for loss or damage that results. Run unknown code only in an isolated environment that you control. Where Forge offers to run code in a virtual machine, you provide that machine and you are responsible for setting it up and keeping it isolated.

## 5. No warranty and 6. Limits of liability

Forge is free, open-source software. You use it, and any code you analyse, open or run with it, at your own risk. Forge is provided "as is" and "as available", without warranties of any kind, express or implied. That includes any promise that it is error-free, secure, or fit for a particular purpose.

To the extent the law allows, we are not liable for indirect or consequential loss, for loss of data, profit or business, or for damage caused by code you analyse, open or run, or by relying on a report. Where the law does not allow a liability to be excluded (for example for intent, gross negligence, or rights that consumer law protects), nothing in these terms excludes it. Because Forge is free, our total liability, to the extent the law allows, is limited to the amount you paid us for Forge, which is nothing if you did not pay. The disclaimer of warranty and limitation of liability in the GNU General Public License also apply to the code. [Lawyer to confirm this limit.]

## 7. Open-source and third-party software, and 8. Privacy

Forge includes open-source and third-party software under their own licences, listed in the app (under View Dependency Licenses) and in the file THIRD-PARTY-NOTICES.md that comes with the analysis engine. We are not responsible for third-party code, packages, templates or websites. Downloading Forge through our website or a package manager is also governed by those services' own terms.

Forge is not affiliated with, endorsed by or sponsored by Zed Industries, Microsoft, GitHub or any other company whose products or names Forge works with or mentions. Those names are trademarks of their owners and are used only to identify them. Forge may be based on or include open-source software, including parts of the Zed editor, used under its open-source licences. Each Forge crate states its own licence in its Cargo.toml: most are under the GNU General Public License, version 3 or later, some under the Apache License 2.0, and one under the MIT licence. [Lawyer to confirm this statement. A few crates still state no licence and will be given one.]

Forge works without an account and does not send your code, projects, reports or usage to us. It stores one small file on your computer recording which version of these terms you accepted, and when. If a feature ever sends data anywhere, it will say so clearly before you use it.

## 9. Contributions and pull requests

When you submit a pull request, template or other contribution, you confirm that you have the right to submit it and you license it to us and to Forge's users under the licence of the part of Forge you contribute to (the GNU General Public License, version 3 or later, unless the file says otherwise). [Decide whether contributors also sign off each commit, for example with a Developer Certificate of Origin.]

Contributions may be checked automatically by Forge's own analysis, including runs in an isolated environment, and reviewed by people before they are accepted. Passing an automated check does not guarantee that a contribution is safe, and a maintainer can decline any contribution for any reason. Do not submit code meant to harm, deceive or collect data without clear disclosure. We may reject such contributions and block the contributor. We are not responsible for contributions made by others.

## 10. Changes to these terms, and 11. Governing law and contact

We may update these terms. Each version has a number and a date. After an update, the app shows a notice. When a change is significant, for example new risks, new handling of data or a new feature that runs code, the app asks you to accept again before you continue. Continuing to use Forge after a minor change means you accept it.

These terms are governed by the law of [country, to be confirmed with a lawyer], and the courts of [place] can decide disputes. This does not take away rights that mandatory consumer law gives you where you live.

Contact: mentenaz@outlook.de. Version 0.1, [date].
