Set-StrictMode -Version Latest

function Invoke-PusulaGhJson {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)][string[]] $Arguments,
        [Parameter(Mandatory = $true)][string] $FailureMessage
    )

    $response = @(& gh @Arguments)
    if ($LASTEXITCODE -ne 0) { throw $FailureMessage }
    $json = ($response -join "`n")
    if ([string]::IsNullOrWhiteSpace($json)) { throw $FailureMessage }
    return $json | ConvertFrom-Json
}

function Assert-PusulaReleaseRepositoryControls {
    [CmdletBinding()]
    param([Parameter(Mandatory = $true)][string] $Repository)

    if ($Repository -notmatch '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$') {
        throw "Invalid GitHub repository name: $Repository"
    }

    $immutability = Invoke-PusulaGhJson `
        -Arguments @(
            'api',
            '-H', 'X-GitHub-Api-Version: 2026-03-10',
            "repos/$Repository/immutable-releases"
        ) `
        -FailureMessage 'Could not verify repository release immutability through GitHub.'
    if (-not [bool]$immutability.enabled) {
        throw 'Repository release immutability must be enabled before privileged release work begins.'
    }

    $summaries = @(Invoke-PusulaGhJson `
            -Arguments @('api', "repos/$Repository/rulesets") `
            -FailureMessage 'Could not enumerate repository rulesets through GitHub.')
    $matching = @($summaries | Where-Object {
            [string]$_.name -ceq 'Protect release tags' -and
            [string]$_.target -ceq 'tag'
        })
    if ($matching.Count -ne 1) {
        throw 'Exactly one tag ruleset named Protect release tags is required.'
    }

    $rulesetId = [long]$matching[0].id
    if ($rulesetId -le 0) { throw 'Protect release tags has an invalid ruleset ID.' }
    $ruleset = Invoke-PusulaGhJson `
        -Arguments @('api', "repos/$Repository/rulesets/$rulesetId") `
        -FailureMessage 'Could not read the Protect release tags ruleset through GitHub.'

    if ([long]$ruleset.id -ne $rulesetId -or
        [string]$ruleset.name -cne 'Protect release tags' -or
        [string]$ruleset.target -cne 'tag' -or
        [string]$ruleset.enforcement -cne 'active') {
        throw 'Protect release tags is not the exact active tag ruleset.'
    }

    $include = @($ruleset.conditions.ref_name.include)
    $exclude = @($ruleset.conditions.ref_name.exclude)
    if ($include.Count -ne 1 -or [string]$include[0] -cne 'refs/tags/v*' -or $exclude.Count -ne 0) {
        throw 'Protect release tags must include only refs/tags/v* and have no exclusions.'
    }

    $ruleTypes = @($ruleset.rules | ForEach-Object { [string]$_.type } | Sort-Object)
    $expectedRuleTypes = @('deletion', 'update')
    if (($ruleTypes -join "`n") -cne ($expectedRuleTypes -join "`n")) {
        throw 'Protect release tags must prohibit update and deletion without restricting creation.'
    }
    if (@($ruleset.bypass_actors).Count -ne 0 -or [string]$ruleset.current_user_can_bypass -cne 'never') {
        throw 'Protect release tags must have no bypass actors and must not be bypassable by the current user.'
    }

    return [pscustomobject][ordered]@{
        immutable_releases = $true
        tag_ruleset_id = $rulesetId
        tag_ruleset_name = 'Protect release tags'
    }
}
