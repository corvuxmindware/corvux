## Default Permission

Default permissions for the iOS picker plugin. Grants folder pick + scoped-access resume/release.

#### This default permission set includes the following:

- `allow-pick-folder`
- `allow-resume-access`
- `allow-release-access`

## Permission Table

<table>
<tr>
<th>Identifier</th>
<th>Description</th>
</tr>


<tr>
<td>

`ios-picker:allow-pick-folder`

</td>
<td>

Enables the pick_folder command — opens iOS's UIDocumentPickerViewController in folder mode and returns a security-scoped bookmark.

</td>
</tr>

<tr>
<td>

`ios-picker:allow-release-access`

</td>
<td>

Enables the release_access command — drops scoped access for a previously resumed folder.

</td>
</tr>

<tr>
<td>

`ios-picker:allow-resume-access`

</td>
<td>

Enables the resume_access command — re-acquires scoped access for a previously picked folder from its persisted bookmark.

</td>
</tr>

<tr>
<td>

`ios-picker:allow-pick-folder`

</td>
<td>

Enables the pick_folder command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`ios-picker:deny-pick-folder`

</td>
<td>

Denies the pick_folder command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`ios-picker:allow-release-access`

</td>
<td>

Enables the release_access command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`ios-picker:deny-release-access`

</td>
<td>

Denies the release_access command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`ios-picker:allow-resume-access`

</td>
<td>

Enables the resume_access command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`ios-picker:deny-resume-access`

</td>
<td>

Denies the resume_access command without any pre-configured scope.

</td>
</tr>
</table>
